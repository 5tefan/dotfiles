//! Pinned GitHub reads, explicit PR transport/checkout, and non-retrying PR mutations.

use crate::model::{ChangedFile, PullRequest};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const API_VERSION: &str = "X-GitHub-Api-Version: 2022-11-28";
const BOOKMARK_TEMPLATE: &str =
    r#"if(!remote && !name.starts_with("zed-pr-review/"), name ++ "\t" ++ conflict ++ "\n")"#;

thread_local! {
    static BACKGROUND_COMMAND_TIMEOUT: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// Limit gh reads and Git fetches through the shared runner to 60 seconds each on
/// the current thread. Other threads and foreground commands are unaffected.
/// Nested scopes restore the
/// previous deadline setting, including when `work` returns an error or unwinds.
/// This is a per-command limit, not a deadline for the entire closure.
pub fn with_background_deadlines<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    with_background_timeout(Duration::from_secs(60), work)
}

fn with_background_timeout<T>(timeout: Duration, work: impl FnOnce() -> Result<T>) -> Result<T> {
    struct RestoreTimeout(Option<Duration>);
    impl Drop for RestoreTimeout {
        fn drop(&mut self) {
            BACKGROUND_COMMAND_TIMEOUT.set(self.0);
        }
    }
    let _restore = RestoreTimeout(BACKGROUND_COMMAND_TIMEOUT.replace(Some(timeout)));
    work()
}

#[derive(Clone, Debug)]
pub struct Client {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub host: String,
    pub repo: String,
    pub reviewer: String,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceRemote {
    name: String,
    url: String,
}

#[derive(Debug)]
struct SourceBookmark {
    remote: String,
    target: Option<String>,
    tracked: bool,
}

struct IgnoreCheck {
    work_tree: PathBuf,
    tracked: HashSet<PathBuf>,
    target: HashSet<PathBuf>,
    excludes_file: Option<PathBuf>,
}

pub(crate) struct ReadyState {
    pub pr: PullRequest,
    pub draft: bool,
    pub node_id: String,
    pub base_ref: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchPrState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchPr {
    pub number: u64,
    pub node_id: String,
    pub state: BranchPrState,
    pub draft: bool,
    pub title: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub url: String,
    pub author: String,
}

struct RecordedRevision {
    commit_id: String,
    empty_parent: Option<String>,
}

impl RecordedRevision {
    fn read(root: &Path, sha: &str) -> Result<Self> {
        validate_sha(sha)?;
        let output = text(run(
            jj_command(root).args([
                "log",
                "-r",
                sha,
                "--no-graph",
                "-T",
                r#"commit_id ++ "\t" ++ conflict ++ "\t" ++ empty ++ "\t" ++ parents.map(|parent| parent.commit_id()).join(" ")"#,
            ]),
            "inspect the recorded jj commit and its parents without snapshotting",
        )?)?;
        let revision = Self::parse(&output)?;
        ensure!(
            revision.commit_id.eq_ignore_ascii_case(sha),
            "jj returned a different commit from the captured revision"
        );
        Ok(revision)
    }

    fn parse(output: &str) -> Result<Self> {
        let fields: Vec<_> = output.trim_end_matches(['\r', '\n']).split('\t').collect();
        ensure!(fields.len() == 4, "Invalid recorded jj revision output");
        validate_sha(fields[0])?;
        let conflict: bool = fields[1].parse().context("Invalid jj conflict flag")?;
        let empty: bool = fields[2].parse().context("Invalid jj empty flag")?;
        let parents: Vec<_> = if fields[3].is_empty() {
            Vec::new()
        } else {
            fields[3].split(' ').collect()
        };
        for parent in &parents {
            validate_sha(parent)?;
        }
        // jj's empty flag compares against the merged parent tree. A merge is
        // never an acceptable wrapper, even when that comparison is empty.
        let empty_parent = match parents.as_slice() {
            [parent] if empty && !conflict => Some(parent.to_ascii_lowercase()),
            _ => None,
        };
        Ok(Self {
            commit_id: fields[0].to_ascii_lowercase(),
            empty_parent,
        })
    }

    fn matches(&self, head: &str) -> bool {
        self.commit_id.eq_ignore_ascii_case(head)
            || self
                .empty_parent
                .as_ref()
                .is_some_and(|parent| parent.eq_ignore_ascii_case(head))
    }
}

impl Client {
    /// Discover a PR at recorded jj `@` or its sole parent when `@` is empty and
    /// conflict-free. Never snapshot, fetch, or check out.
    /// An unqualified `--repo owner/repo` uses github.com unless a PR URL supplies a host.
    pub fn discover(
        directory: &Path,
        pr: Option<&str>,
        repo: Option<&str>,
    ) -> Result<(Self, PullRequest)> {
        let (client, pull) = Self::discover_revision(directory, pr, repo, None)?;
        client.ensure_checkout(&pull)?;
        Ok((client, pull))
    }

    /// Discover a PR for a previously captured jj `@`, even if the checkout moves.
    /// The caller must capture the actual `head_sha` before discovery. It must
    /// equal the PR head or be its empty, conflict-free single-parent child;
    /// no live `@` check follows.
    pub fn discover_at(
        directory: &Path,
        pr: Option<&str>,
        repo: Option<&str>,
        head_sha: &str,
    ) -> Result<(Self, PullRequest)> {
        Self::discover_revision(directory, pr, repo, Some(head_sha))
    }

    fn discover_revision(
        directory: &Path,
        pr: Option<&str>,
        repo: Option<&str>,
        captured_head: Option<&str>,
    ) -> Result<(Self, PullRequest)> {
        let explicit_pr = pr.map(parse_pr_spec).transpose()?;
        let explicit_repo = repo.map(parse_repo_spec).transpose()?;
        // Reject conflicting explicit arguments before invoking any subprocess.
        let explicit_target = explicit_target(explicit_pr.as_ref(), explicit_repo.as_ref())?;
        let root = workspace_root(directory)?;
        let head_sha = match captured_head {
            Some(sha) => {
                validate_sha(sha)?;
                sha.to_ascii_lowercase()
            }
            None => checkout_sha(&root)?,
        };
        // Capture the relationship before any GitHub call. Later checkout moves
        // must not change discover_at's interpretation of the initial revision.
        let revision = RecordedRevision::read(&root, &head_sha)?;
        let lookup = if explicit_pr.is_none() {
            Some(select_discovery_bookmark(&revision, |sha| {
                text(run(
                    jj_command(&root).args([
                        "bookmark",
                        "list",
                        "-r",
                        sha,
                        "--template",
                        BOOKMARK_TEMPLATE,
                    ]),
                    "list local bookmarks at the captured jj revision",
                )?)
            })?)
        } else {
            None
        };
        let mut client = Self::in_workspace(&root, explicit_target)?;
        let listed = if let Some((lookup_sha, bookmark)) = &lookup {
            let response = run(
                gh_command(&client.root, Some(&client.host)).args([
                    "pr",
                    "list",
                    "--repo",
                    &client.repo_argument(),
                    "--head",
                    bookmark,
                    "--state",
                    "open",
                    "--limit",
                    "100",
                    "--json",
                    "number,headRefOid,url,headRepository,headRepositoryOwner",
                ]),
                "find an open PR for the local bookmark",
            )?;
            let candidates: Vec<ListedPr> =
                serde_json::from_slice(&response).context("Invalid gh pr list JSON")?;
            Some(select_pr(candidates, lookup_sha, &client.target())?)
        } else {
            None
        };
        let number = match (&explicit_pr, &listed) {
            (Some(spec), _) => spec.number,
            (_, Some(candidate)) => candidate.number,
            _ => bail!("No PR was selected"),
        };
        let pull = client.read_pr(number)?;
        ensure!(
            revision.matches(&pull.head_sha),
            "Captured jj @ {} is neither PR head {} nor its empty, conflict-free single-parent child; open the PR again",
            head_sha,
            pull.head_sha
        );
        if let (Some((lookup_sha, bookmark)), Some(listed)) = (&lookup, &listed) {
            ensure!(
                pull.head_sha == *lookup_sha,
                "PR head changed during discovery; retry with an explicit --pr"
            );
            ensure!(
                pull.head_ref == *bookmark
                    && pull.head_repo.eq_ignore_ascii_case(&listed.head_repo),
                "PR source changed during discovery; retry with an explicit --pr"
            );
        }
        client.reviewer = pull.reviewer.clone();
        client.check_identity(&pull)?;
        Ok((client, pull))
    }

    /// Resolve an explicit PR independently of the recorded `@` and local bookmarks.
    /// This does not fetch, snapshot, or change the working copy.
    pub fn resolve(directory: &Path, pr: &str, repo: Option<&str>) -> Result<(Self, PullRequest)> {
        let spec = parse_pr_spec(pr)?;
        let repo = repo.map(parse_repo_spec).transpose()?;
        let target = explicit_target(Some(&spec), repo.as_ref())?;
        let mut client = Self::in_workspace(directory, target)?;
        let pull = client.read_pr(spec.number)?;
        client.reviewer = pull.reviewer.clone();
        Ok((client, pull))
    }

    /// Resolve the repository and authenticated GitHub user for a workspace
    /// without requiring an existing PR on `@`.
    pub fn for_workspace(directory: &Path, repo: Option<&str>) -> Result<Self> {
        let repo = repo.map(parse_repo_spec).transpose()?;
        let target = explicit_target(None, repo.as_ref())?;
        let mut client = Self::in_workspace(directory, target)?;
        client.reviewer = client.authenticated_reviewer()?;
        Ok(client)
    }

    fn in_workspace(directory: &Path, target: Option<Target>) -> Result<Self> {
        let root = workspace_root(directory)?;
        let git_dir = command_path(
            run(
                jj_command(&root).args(["git", "root"]),
                "resolve jj Git directory",
            )?,
            &root,
        )?;
        let target = match target {
            Some(target) => target,
            None => {
                // gh must use jj's backing repository, including non-colocated workspaces.
                let response = run(
                    gh_command(&root, None).env("GIT_DIR", &git_dir).args([
                        "repo",
                        "view",
                        "--json",
                        "nameWithOwner,url",
                    ]),
                    "discover GitHub repository (or supply --repo [HOST/]owner/repo)",
                )?;
                map_repo_view(
                    &serde_json::from_slice(&response).context("Invalid gh repo view JSON")?,
                )?
            }
        };
        Ok(Self {
            root,
            git_dir,
            host: target.host,
            repo: target.repo,
            reviewer: String::new(),
        })
    }

    /// Read the current open PR and authenticated user, without requiring a checkout match.
    pub fn read_pr(&self, number: u64) -> Result<PullRequest> {
        let endpoint = self.pr_endpoint(number)?;
        let response = self.get(&endpoint)?;
        let reviewer = self.authenticated_reviewer()?;
        map_pr(&response, &self.target(), number, &reviewer)
    }

    pub(crate) fn read_ready_state(&self, number: u64) -> Result<ReadyState> {
        let response = self.get(&self.pr_endpoint(number)?)?;
        let reviewer = self.authenticated_reviewer()?;
        let node_id = required_string(&response, "node_id")?;
        let base_ref = required_string(&response["base"], "ref")?;
        ensure!(
            !node_id.trim().is_empty(),
            "PR response has an empty node_id"
        );
        ensure!(
            !base_ref.trim().is_empty(),
            "PR response has an empty base ref"
        );
        Ok(ReadyState {
            pr: map_pr(&response, &self.target(), number, &reviewer)?,
            draft: response["draft"]
                .as_bool()
                .context("PR response is missing a boolean draft field")?,
            node_id: node_id.to_owned(),
            base_ref: base_ref.to_owned(),
        })
    }

    pub(crate) fn mark_ready(&self, state: &ReadyState) -> Result<()> {
        self.check_identity(&state.pr)?;
        self.mark_pr_ready(&state.node_id)
    }

    /// Mark a draft pull request ready for review via GitHub's GraphQL API.
    pub fn mark_pr_ready(&self, node_id: &str) -> Result<()> {
        self.validate_target()?;
        ensure!(
            !node_id.trim().is_empty(),
            "PR response has an empty node_id"
        );
        let response = self.post_json("graphql", &json!({
            "query": "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { id isDraft } } }",
            "variables": {"id": node_id},
        }))?;
        let pr = &response["data"]["markPullRequestReadyForReview"]["pullRequest"];
        ensure!(
            response.get("errors").is_none() && pr["id"] == node_id && pr["isDraft"] == false,
            "Unexpected mark-ready response; outcome is uncertain, check GitHub before retrying"
        );
        Ok(())
    }

    /// List all PRs (open, closed, merged) for a head branch in this repository.
    pub fn list_prs_for_head(&self, head: &str) -> Result<Vec<BranchPr>> {
        self.validate_target()?;
        validate_bookmark(head)?;
        let owner = self
            .repo
            .split_once('/')
            .map(|(owner, _)| owner)
            .context("Repository must be OWNER/REPO")?;
        let head_filter = percent_encode_query_value(&format!("{owner}:{head}"));
        let endpoint = format!(
            "repos/{}/pulls?state=all&head={head_filter}&per_page=100",
            self.repo
        );
        let items: Vec<Value> = self.get_pages(&endpoint)?;
        items
            .iter()
            .map(|item| map_branch_pr(item, &self.target()))
            .collect()
    }

    /// Create a new PR in this repository.
    pub fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<BranchPr> {
        self.validate_target()?;
        validate_bookmark(head)?;
        validate_bookmark(base)?;
        let endpoint = format!("repos/{}/pulls", self.repo);
        let payload = json!({
            "head": head,
            "base": base,
            "title": title,
            "body": body,
            "draft": draft,
        });
        let response = self.send_json("POST", &endpoint, &payload)?;
        map_branch_pr(&response, &self.target())
    }

    /// Update an existing PR's base, title, and/or state. Never touches `body`.
    pub fn update_pr(
        &self,
        number: u64,
        base: Option<&str>,
        title: Option<&str>,
        state: Option<&str>,
    ) -> Result<BranchPr> {
        let endpoint = self.pr_endpoint(number)?;
        let mut map = serde_json::Map::new();
        if let Some(base) = base {
            validate_bookmark(base)?;
            map.insert("base".to_owned(), Value::String(base.to_owned()));
        }
        if let Some(title) = title {
            map.insert("title".to_owned(), Value::String(title.to_owned()));
        }
        if let Some(state) = state {
            map.insert("state".to_owned(), Value::String(state.to_owned()));
        }
        ensure!(
            !map.is_empty(),
            "update_pr requires at least one field to update"
        );
        let response = self.send_json("PATCH", &endpoint, &Value::Object(map))?;
        map_branch_pr(&response, &self.target())
    }

    pub(crate) fn request_reviewer(&self, pr: &PullRequest, reviewer: &str) -> Result<()> {
        let payload = reviewer_payload(&pr.repo, reviewer)?;
        self.check_identity(pr)?;
        // REST adds requests without replacing concurrent requests by other users.
        let endpoint = format!("{}/requested_reviewers", self.pr_endpoint(pr.number)?);
        self.post_json(&endpoint, &payload)?;
        Ok(())
    }

    /// Authenticate recovery without requiring an open PR or a local checkout.
    pub fn authenticated_reviewer(&self) -> Result<String> {
        let user = self.get("user")?;
        let reviewer = required_string(&user, "login")?;
        validate_login(reviewer)?;
        ensure!(
            self.reviewer.is_empty() || self.reviewer.eq_ignore_ascii_case(reviewer),
            "Authenticated GitHub user changed; rediscover the PR before continuing"
        );
        Ok(reviewer.to_owned())
    }

    /// Check identity and whether recorded jj `@` is the pinned PR head or its
    /// empty, conflict-free single-parent child, without snapshotting or importing
    /// refs. Inspection failures are errors, not checkout mismatches.
    pub fn checkout_matches(&self, pr: &PullRequest) -> Result<bool> {
        self.check_identity(pr)?;
        let actual = checkout_sha(&self.root)?;
        Ok(RecordedRevision::read(&self.root, &actual)?.matches(&pr.head_sha))
    }

    /// Require a matching recorded checkout without mutating the workspace.
    pub fn ensure_checkout(&self, pr: &PullRequest) -> Result<()> {
        ensure!(
            self.checkout_matches(pr)?,
            "Recorded jj @ is neither the pinned PR head {} nor its empty, conflict-free single-parent child; open the PR again",
            pr.head_sha
        );
        Ok(())
    }

    /// Read the recorded working-copy commit without snapshotting or importing Git refs.
    pub fn checkout_commit(&self) -> Result<String> {
        checkout_sha(&self.root)
    }

    /// Fetch pinned history without invoking jj, importing refs, taking a workspace/draft
    /// lock, or changing files. The caller supplies the base branch from its live PR read.
    /// These refs are outside jj's import namespaces and, like foreground transport refs,
    /// intentionally remain reachable after use; no automatic eviction is performed.
    pub fn prefetch_pr(&self, pr: &PullRequest, base_ref: &str) -> Result<()> {
        self.check_identity(pr)?;
        validate_bookmark(base_ref)?;
        let source = self.source_remote(pr)?;
        let namespace = self.prefetch_namespace(pr, base_ref, source.as_ref())?;
        if self
            .prefetched_base(pr, base_ref, &namespace, source.is_some())?
            .is_some()
        {
            return Ok(());
        }
        // A cache miss can leave old refs, including a partial fork fetch. Never let
        // fetch dereference one of those names into a user branch (even if dangling).
        for suffix in ["head", "base", "source"] {
            let symbolic = self
                .git_command()
                .args(["symbolic-ref", "--quiet", &format!("{namespace}/{suffix}")])
                .output()?;
            ensure!(
                symbolic.status.code() == Some(1),
                "Cannot prefetch over a symbolic or unreadable cache ref"
            );
        }
        let url = format!("https://{}/{}.git", self.host, self.repo);
        self.check_transport_url(&self.repo, &url)?;
        if let Some(source) = &source {
            self.check_transport_url(&pr.head_repo, &source.url)?;
        }
        let shallow = text(run(
            self.git_command()
                .args(["rev-parse", "--is-shallow-repository"]),
            "inspect repository history depth",
        )?)?;
        let mut fetch = self.pr_fetch_command_for(&self.repo);
        if shallow.trim() == "true" {
            fetch.arg("--unshallow");
        }
        fetch.args([
            "--",
            &url,
            &format!("refs/pull/{}/head:{namespace}/head", pr.number),
            &format!("refs/heads/{base_ref}:{namespace}/base"),
        ]);
        let same_repo = pr.head_repo.eq_ignore_ascii_case(&self.repo);
        if source.is_some() && same_repo {
            fetch.arg(format!("refs/heads/{}:{namespace}/source", pr.head_ref));
        }
        run(
            &mut fetch,
            "prefetch the pinned PR and base history (check gh authentication)",
        )?;
        if let Some(source) = source.as_ref().filter(|_| !same_repo) {
            run(
                self.pr_fetch_command_for(&pr.head_repo).args([
                    "--",
                    &source.url,
                    &format!("refs/heads/{}:{namespace}/source", pr.head_ref),
                ]),
                "prefetch only the PR source branch",
            )?;
        }
        ensure!(
            self.prefetched_base(pr, base_ref, &namespace, source.is_some())?
                .is_some(),
            "Prefetched head/source does not match the pinned PR, or base does not contain the pinned commit; resolve the PR again"
        );
        self.merge_base(pr)?;
        ensure!(
            self.source_remote(pr)? == source,
            "PR source remote changed during prefetch; retry"
        );
        Ok(())
    }

    fn prefetch_namespace(
        &self,
        pr: &PullRequest,
        base_ref: &str,
        source: Option<&SourceRemote>,
    ) -> Result<String> {
        // Length-delimited JSON avoids ambiguous concatenation. Cosmetic PR fields and
        // the reviewer do not affect transport; source configuration affects tracking.
        let key = serde_json::to_vec(&(
            1,
            self.host.to_ascii_lowercase(),
            self.repo.to_ascii_lowercase(),
            pr.number,
            pr.head_sha.to_ascii_lowercase(),
            pr.base_sha.to_ascii_lowercase(),
            pr.head_repo.to_ascii_lowercase(),
            &pr.head_ref,
            base_ref,
            source.map(|source| (&source.name, &source.url)),
        ))?;
        Ok(format!(
            "refs/zed-pr-review-prefetch/{:x}",
            Sha256::digest(key)
        ))
    }

    fn prefetched_base(
        &self,
        pr: &PullRequest,
        base_ref: &str,
        namespace: &str,
        has_source: bool,
    ) -> Result<Option<String>> {
        // Inspect only this key, in one Git process, rather than enumerating and probing
        // each retained PR. Missing, symbolic, non-commit, or stale entries are misses.
        let listing = text(run(
            self.git_command().args([
                "for-each-ref",
                "--format=%(refname)%09%(objectname)%09%(objecttype)%09%(symref)",
                &format!("{namespace}/"),
            ]),
            "inspect prefetched PR refs",
        )?)?;
        let mut refs = BTreeMap::new();
        for line in listing.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(fields.len() == 4, "Unexpected prefetched Git ref state");
            if fields[2] == "commit" && fields[3].is_empty() {
                validate_sha(fields[1])?;
                refs.insert(fields[0], fields[1]);
            }
        }
        for suffix in if has_source {
            &["head", "source"][..]
        } else {
            &["head"][..]
        } {
            if !refs
                .get(format!("{namespace}/{suffix}").as_str())
                .is_some_and(|sha| sha.eq_ignore_ascii_case(&pr.head_sha))
            {
                return Ok(None);
            }
        }
        let Some(base) = refs.get(format!("{namespace}/base").as_str()) else {
            return Ok(None);
        };
        if self.check_base_history(pr, base_ref, base).is_err() {
            return Ok(None);
        }
        Ok(Some((*base).to_owned()))
    }

    fn publish_prefetched_refs(&self, refs: &[(String, String)]) -> Result<()> {
        // Create all refs atomically, without dereferencing or overwriting any existing
        // name. Publish verified object IDs, not cache ref names that could change.
        let input: String = refs
            .iter()
            .map(|(reference, sha)| format!("create {reference} {sha}\n"))
            .collect();
        let mut child = self
            .git_command()
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "--no-deref",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Cannot publish prefetched PR refs")?;
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        successful_output(
            child.wait_with_output()?,
            "publish prefetched PR refs locally",
        )?;
        Ok(())
    }

    /// Fetch the pinned PR/base into private refs, including forks without a source remote.
    /// If exactly one configured remote identifies the source repository, also fetch only
    /// its source branch, verify the pinned head, and track it. Refuse ambiguous remotes,
    /// conflicting local bookmarks, pending Git ref changes, and non-fast-forward updates.
    ///
    /// Use hardened HTTPS fetch/import rather than `jj git fetch`: configured transports,
    /// refspecs, URL rewrites, and automatic tracking must not widen this operation.
    /// No snapshot or working-copy update occurs here. Serialize this with other repository
    /// operations. The caller must prepare storage against `pr.head_sha` before checkout.
    pub fn fetch_pr(&self, pr: &PullRequest) -> Result<()> {
        self.check_identity(pr)?;
        let base_ref = self.pinned_base_ref(pr)?;
        let before = self.checkout_commit()?;
        let source = self.source_remote(pr)?;
        self.check_import_refs(&[])?;
        let cache_namespace = self.prefetch_namespace(pr, &base_ref, source.as_ref())?;
        let cached_base =
            self.prefetched_base(pr, &base_ref, &cache_namespace, source.is_some())?;
        let url = format!("https://{}/{}.git", self.host, self.repo);
        self.check_transport_url(&self.repo, &url)?;
        let token = transport_token(&self.git_dir)?;
        let namespace = token
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .context("Invalid transport token")?;
        let head_ref = format!("refs/heads/zed-pr-review/{namespace}/head");
        let fetched_base = format!("refs/heads/zed-pr-review/{namespace}/base");

        let existing = run(
            self.git_command().args([
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/heads/zed-pr-review/{namespace}"),
            ]),
            "verify that the PR fetch namespace is unused",
        )?;
        ensure!(
            existing.is_empty(),
            "PR fetch namespace already exists; no refs were overwritten, retry"
        );
        if let Some(actual_base) = &cached_base {
            let mut refs = vec![
                (head_ref.clone(), pr.head_sha.clone()),
                (fetched_base.clone(), actual_base.clone()),
            ];
            if let Some(source) = &source {
                self.check_transport_url(&pr.head_repo, &source.url)?;
                refs.push((
                    format!("refs/heads/zed-pr-review/{namespace}/source"),
                    pr.head_sha.clone(),
                ));
            }
            self.publish_prefetched_refs(&refs)?;
        } else {
            let shallow = text(run(
                self.git_command()
                    .args(["rev-parse", "--is-shallow-repository"]),
                "inspect repository history depth",
            )?)?;
            let mut fetch = self.pr_fetch_command_for(&self.repo);
            if shallow.trim() == "true" {
                fetch.arg("--unshallow");
            }
            fetch.args([
                "--",
                &url,
                &format!("refs/pull/{}/head:{head_ref}", pr.number),
                &format!("refs/heads/{base_ref}:{fetched_base}"),
            ]);
            run(
                &mut fetch,
                "fetch the pinned PR and base history (check gh authentication)",
            )?;
        }
        let actual_head = self
            .git_ref_target(&head_ref)?
            .context("Fetched PR head ref is missing")?;
        ensure!(
            actual_head.eq_ignore_ascii_case(&pr.head_sha),
            "PR #{} head moved during fetch: expected {}, fetched {}. Resolve the PR again before opening it",
            pr.number,
            pr.head_sha,
            actual_head
        );
        let actual_base = self
            .git_ref_target(&fetched_base)?
            .context("Fetched PR base ref is missing")?;
        self.check_base_history(pr, &base_ref, &actual_base)?;
        let mut imported_refs = vec![
            (head_ref.clone(), pr.head_sha.clone()),
            (fetched_base.clone(), actual_base),
        ];
        if let Some(source) = &source {
            let source_ref = format!("refs/heads/zed-pr-review/{namespace}/source");
            if cached_base.is_none() {
                self.check_transport_url(&pr.head_repo, &source.url)?;
                run(
                    self.pr_fetch_command_for(&pr.head_repo).args([
                        "--",
                        &source.url,
                        &format!("refs/heads/{}:{source_ref}", pr.head_ref),
                    ]),
                    "fetch only the PR source branch into a private ref",
                )?;
            }
            ensure!(
                self.git_ref_target(&source_ref)?.as_deref() == Some(pr.head_sha.as_str()),
                "PR source branch does not match the pinned PR head; resolve the PR again"
            );
            imported_refs.push((source_ref, pr.head_sha.clone()));
            self.check_source_tracking(pr, source)?;
        }
        self.merge_base(pr)?;
        ensure!(
            self.pinned_base_ref(pr)? == base_ref,
            "PR base branch changed during fetch; resolve the PR again"
        );
        // Local refs work in both colocated and non-colocated jj stores. Untracked
        // remote refs would make the PR immutable under jj's default configuration.
        // A global import also merges already-tracked refs, regardless of auto-track
        // and abandonment settings. Recheck after transport, allowing only our exact refs.
        self.import_pr_refs(
            &imported_refs,
            "import the fetched PR into jj without updating the working copy",
        )?;
        ensure!(
            self.source_remote(pr)? == source,
            "PR source remote changed during fetch; retry"
        );
        if let Some(source) = &source {
            self.track_source(pr, source)?;
        }
        self.commit_state(&pr.head_sha)?;
        ensure!(
            self.checkout_commit()? == before,
            "jj @ changed during fetch; inspect the workspace before opening the PR"
        );
        if cached_base.is_none() {
            // Keep the first foreground open warm too, so returning to it does not
            // fetch again. Another worker may have published the same cache meanwhile.
            let cache_refs: Vec<_> = imported_refs
                .iter()
                .map(|(reference, sha)| {
                    (
                        format!(
                            "{cache_namespace}/{}",
                            reference.rsplit('/').next().unwrap()
                        ),
                        sha.clone(),
                    )
                })
                .collect();
            let _ = self.publish_prefetched_refs(&cache_refs);
        }
        Ok(())
    }

    fn check_base_history(
        &self,
        pr: &PullRequest,
        base_ref: &str,
        actual_base: &str,
    ) -> Result<()> {
        if !actual_base.eq_ignore_ascii_case(&pr.base_sha) {
            // GitHub's PR base SHA can lag behind the branch tip. Fetching the
            // branch supplies its history; it must not change our pinned diff.
            let ancestor = self
                .git_command()
                .args(["merge-base", "--is-ancestor", &pr.base_sha, actual_base])
                .output()
                .context("Cannot check the fetched base history")?;
            ensure!(
                ancestor.status.code() != Some(1),
                "Fetched base branch {base_ref} at {actual_base} does not contain pinned PR base {}. The branch was rewound or diverged; resolve the PR again before opening it",
                pr.base_sha
            );
            successful_output(
                ancestor,
                "check that the fetched base contains the pinned commit",
            )
            .with_context(|| {
                format!(
                    "Cannot verify pinned PR base {} against fetched {base_ref} at {actual_base}",
                    pr.base_sha
                )
            })?;
        }
        Ok(())
    }

    fn source_remote(&self, pr: &PullRequest) -> Result<Option<SourceRemote>> {
        validate_repo(&pr.head_repo)?;
        validate_bookmark(&pr.head_ref)?;
        let names = text(run(
            self.git_command().arg("remote"),
            "list configured Git remotes",
        )?)?;
        let mut matches = Vec::new();
        for name in names.lines() {
            let output = self
                .git_command()
                .args(["config", "--get-all", &format!("remote.{name}.url")])
                .output()?;
            if output.status.code() == Some(1) {
                continue;
            }
            let urls = text(successful_output(output, "read the configured remote URL")?)?;
            let urls: Vec<_> = urls.lines().collect();
            if !urls.iter().any(|url| source_url_matches(url, pr)) {
                continue;
            }
            ensure!(
                urls.len() == 1,
                "Source remote {name} has multiple URLs; choose one manually"
            );
            validate_bookmark(name)?;
            ensure!(
                !name.contains(['/', '@']) && name != "git",
                "Unsupported source remote name: {name}"
            );
            matches.push(SourceRemote {
                name: name.to_owned(),
                // Remote configuration establishes identity, never the transport or refspec.
                url: format!("https://{}/{}.git", self.host, pr.head_repo),
            });
        }
        ensure!(
            matches.len() <= 1,
            "Multiple remotes match the PR source repository; select tracking manually"
        );
        Ok(matches.pop())
    }

    fn source_bookmarks(&self, branch: &str) -> Result<Vec<SourceBookmark>> {
        let listing = text(run(
            jj_command(&self.root).args([
                "bookmark", "list", "--all-remotes", &format!("exact:{}", serde_json::to_string(branch)?), "-T",
                r#"json(remote) ++ "\t" ++ conflict ++ "\t" ++ if(normal_target, normal_target.commit_id()) ++ "\t" ++ tracked ++ "\n""#,
            ]),
            "inspect local and remote source bookmarks without snapshotting",
        )?)?;
        listing
            .lines()
            .map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                ensure!(fields.len() == 4, "Unexpected jj source bookmark state");
                ensure!(
                    fields[1] == "false",
                    "Source bookmark {branch} is conflicted; resolve it manually"
                );
                ensure!(
                    matches!(fields[3], "true" | "false"),
                    "Unexpected jj tracking state"
                );
                let target = if fields[2].is_empty() {
                    None
                } else {
                    validate_sha(fields[2])?;
                    Some(fields[2].to_owned())
                };
                Ok(SourceBookmark {
                    remote: serde_json::from_str::<Option<String>>(fields[0])?.unwrap_or_default(),
                    target,
                    tracked: fields[3] == "true",
                })
            })
            .collect()
    }

    fn git_ref_target(&self, reference: &str) -> Result<Option<String>> {
        let refs = text(run(
            self.git_command().args([
                "for-each-ref",
                "--format=%(refname)%09%(objectname)%09%(symref)",
                reference,
            ]),
            "inspect the exact Git ref",
        )?)?;
        for line in refs.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(fields.len() == 3, "Unexpected Git ref state");
            if fields[0] == reference {
                ensure!(
                    fields[2].is_empty(),
                    "Refusing symbolic source ref {reference}"
                );
                validate_sha(fields[1])?;
                return Ok(Some(fields[1].to_owned()));
            }
        }
        // for-each-ref omits dangling symbolic refs. They still occupy the name
        // and must not be mistaken for an unused destination.
        let symbolic = self
            .git_command()
            .args(["symbolic-ref", "--quiet", reference])
            .output()?;
        ensure!(
            !symbolic.status.success(),
            "Refusing symbolic source ref {reference}"
        );
        ensure!(
            symbolic.status.code() == Some(1),
            "Cannot inspect source ref {reference}"
        );
        Ok(None)
    }

    fn check_source_tracking(
        &self,
        pr: &PullRequest,
        source: &SourceRemote,
    ) -> Result<Option<String>> {
        let bookmarks = self.source_bookmarks(&pr.head_ref)?;
        let local = bookmarks
            .iter()
            .find(|b| b.remote.is_empty())
            .and_then(|b| b.target.as_deref());
        let remote = bookmarks.iter().find(|b| b.remote == source.name);
        let old = remote.and_then(|b| b.target.as_deref());
        let tracked = remote.is_some_and(|b| b.tracked);
        ensure!(
            !bookmarks.iter().any(|b| b.tracked
                && !b.remote.is_empty()
                && b.remote != "git"
                && b.remote != source.name),
            "Source bookmark {} already tracks another remote; resolve tracking manually",
            pr.head_ref
        );
        let git_local = self.git_ref_target(&format!("refs/heads/{}", pr.head_ref))?;
        if let Some(imported) = bookmarks.iter().find(|b| b.remote == "git") {
            ensure!(
                git_local.as_deref() == imported.target.as_deref(),
                "Source bookmark has pending Git changes; import and resolve them manually"
            );
        }
        ensure!(
            git_local
                .as_deref()
                .is_none_or(|target| Some(target) == local),
            "Source bookmark has pending Git changes; import and resolve them manually"
        );
        ensure!(
            self.git_ref_target(&format!("refs/remotes/{}/{}", source.name, pr.head_ref))?
                .as_deref()
                == old,
            "Source remote ref has pending Git changes; import and resolve them manually"
        );
        ensure!(
            !tracked || local.is_some(),
            "Source bookmark was deleted locally; restore it manually before tracking"
        );
        ensure!(
            local.is_none() || local == Some(pr.head_sha.as_str()) || (tracked && local == old),
            "Local source bookmark {} differs from the pinned head; resolve it manually",
            pr.head_ref
        );
        if let Some(old) = old.filter(|old| *old != pr.head_sha) {
            let ancestor = self
                .git_command()
                .args(["merge-base", "--is-ancestor", old, &pr.head_sha])
                .output()?;
            ensure!(
                ancestor.status.success(),
                "Source branch update is not a fast-forward; resolve tracking manually"
            );
        }
        Ok(old.map(str::to_owned))
    }

    /// Refuse pending Git changes before a global import can move any user bookmark.
    /// Compare imported Git state (including @git), not local jj targets: local edits
    /// and unexported bookmarks are valid and must not be mistaken for Git changes.
    fn check_import_refs(&self, allowed: &[(String, String)]) -> Result<()> {
        let mut imported = BTreeMap::new();
        for kind in ["bookmark", "tag"] {
            let listing = text(run(
                jj_command(&self.root).args([
                    kind, "list", "--all-remotes", "-T",
                    r#"if(remote, json(name) ++ "\t" ++ json(remote) ++ "\t" ++ conflict ++ "\t" ++ if(normal_target, normal_target.commit_id()) ++ "\n")"#,
                ]),
                "inspect jj's imported Git refs without snapshotting",
            )?)?;
            for line in listing.lines() {
                let fields: Vec<_> = line.split('\t').collect();
                ensure!(fields.len() == 4, "Unexpected imported jj ref state");
                let name: String = serde_json::from_str(fields[0])?;
                let remote: String = serde_json::from_str(fields[1])?;
                let reference = match (kind, remote.as_str()) {
                    ("bookmark", "git") => format!("refs/heads/{name}"),
                    ("bookmark", _) => format!("refs/remotes/{remote}/{name}"),
                    ("tag", "git") => format!("refs/tags/{name}"),
                    // Other jj remote tags aren't imported from Git's local tag refs.
                    _ => continue,
                };
                ensure!(
                    fields[2] == "false",
                    "Imported Git ref {reference} is conflicted; resolve it manually before fetching a PR"
                );
                if !fields[3].is_empty() {
                    validate_sha(fields[3])?;
                    ensure!(
                        imported.insert(reference, fields[3].to_owned()).is_none(),
                        "Duplicate imported Git ref"
                    );
                }
            }
        }
        let listing = text(run(
            self.git_command().args([
                "for-each-ref",
                "--format=%(refname)%09%(objectname)%09%(objecttype)%09%(symref)",
                "refs/heads/",
                "refs/remotes/",
                "refs/tags/",
            ]),
            "inspect all Git refs that a global jj import could apply",
        )?)?;
        let mut actual = BTreeMap::new();
        for line in listing.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(fields.len() == 4, "Unexpected Git ref state before import");
            let reference = fields[0];
            // Git's remote default-branch pointer is not a jj remote bookmark.
            if reference.starts_with("refs/remotes/") && reference.ends_with("/HEAD") {
                continue;
            }
            ensure!(
                fields[3].is_empty(),
                "Refusing symbolic source ref {reference} during PR import"
            );
            let target = match fields[2] {
                "commit" => fields[1].to_owned(),
                "tag" => text(run(
                    self.git_command().args([
                        "rev-parse",
                        "--verify",
                        &format!("{reference}^{{commit}}"),
                    ]),
                    "resolve an annotated Git tag before import",
                )?)?
                .trim()
                .to_owned(),
                _ => bail!(
                    "Git ref {reference} does not resolve to a commit; cannot safely import it"
                ),
            };
            validate_sha(&target)?;
            actual.insert(reference.to_owned(), target);
        }
        for (reference, target) in allowed {
            ensure!(
                actual.get(reference) == Some(target),
                "Verified PR ref {reference} changed before import; retry"
            );
            actual.remove(reference);
            imported.remove(reference);
        }
        for reference in imported.keys().chain(actual.keys()) {
            ensure!(
                imported.get(reference) == actual.get(reference),
                "Git ref {reference} has pending Git changes; import and resolve them manually before fetching a PR"
            );
        }
        Ok(())
    }

    fn import_pr_refs(&self, allowed: &[(String, String)], operation: &str) -> Result<()> {
        let mut command = self.transport_jj_command()?;
        // This is a preflight, not a rollback. As with checkout, callers must serialize
        // repository operations; do not run Git/jj concurrently with the check/import.
        self.check_import_refs(allowed)?;
        run(command.args(["git", "import"]), operation)?;
        Ok(())
    }

    fn transport_jj_command(&self) -> Result<Command> {
        let mut command = jj_command(&self.root);
        command.args([
            "--config",
            "git.abandon-unreachable-commits=false",
            "--config",
            "git.auto-local-bookmark=false",
            "--config",
            "git.record-synthetic-predecessors=false",
        ]);
        // The legacy global switch does not disable modern per-remote auto-tracking.
        // Override effective rules for this command only, including unrelated remotes
        // whose refs may already be pending import. An empty exact name matches no ref.
        let keys = text(run(
            jj_command(&self.root).args(["config", "list", "remotes", "-T", "name ++ \"\\n\""]),
            "inspect automatic bookmark tracking rules",
        )?)?;
        for key in keys.lines().filter(|key| {
            key.ends_with(".auto-track-bookmarks") || key.ends_with(".auto-track-created-bookmarks")
        }) {
            command.args(["--config", &format!("{key}='exact:\"\"'")]);
        }
        Ok(command)
    }

    fn track_source(&self, pr: &PullRequest, source: &SourceRemote) -> Result<()> {
        let old = self.check_source_tracking(pr, source)?;
        let reference = format!("refs/remotes/{}/{}", source.name, pr.head_ref);
        self.check_import_refs(&[])?;
        // Compare-and-swap, without dereferencing symbolic refs or running user hooks.
        // Never publish an unverified branch tip under the configured remote's name.
        run(
            self.git_command().args([
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "--no-deref",
                &reference,
                &pr.head_sha,
                old.as_deref().unwrap_or(""),
            ]),
            "record the verified source branch without overwriting a concurrent ref update",
        )?;
        self.import_pr_refs(
            &[(reference, pr.head_sha.clone())],
            "import the verified source branch",
        )?;
        let bookmarks = self.source_bookmarks(&pr.head_ref)?;
        ensure!(
            bookmarks
                .iter()
                .filter(|b| b.remote.is_empty())
                .all(|b| b.target.as_deref() == Some(pr.head_sha.as_str())),
            "Local source bookmark changed during import; resolve it manually"
        );
        run(
            self.transport_jj_command()?.args([
                "bookmark",
                "track",
                &format!("exact:{}", serde_json::to_string(&pr.head_ref)?),
                "--remote",
                &format!("exact:{}", serde_json::to_string(&source.name)?),
            ]),
            "track only the verified PR source branch",
        )?;
        let bookmarks = self.source_bookmarks(&pr.head_ref)?;
        ensure!(
            bookmarks
                .iter()
                .any(|b| b.remote.is_empty() && b.target.as_deref() == Some(pr.head_sha.as_str()))
                && bookmarks.iter().any(|b| b.remote == source.name
                    && b.tracked
                    && b.target.as_deref() == Some(pr.head_sha.as_str())),
            "Source tracking did not resolve to the pinned PR head; inspect bookmarks manually"
        );
        Ok(())
    }

    /// Snapshot current work, then edit a mutable PR head or create an empty
    /// child of an immutable head without overriding it. Creates no bookmarks.
    /// Returns the previous commit ID for recovery, including an empty commit
    /// that jj may hide when switching away. Never forces a conflicted checkout.
    /// Sparse workspaces, submodules, and non-UTF-8 paths are conservatively refused.
    /// The caller must serialize review operations; do not run other jj commands while switching.
    pub fn checkout_pr(&self, pr: &PullRequest) -> Result<String> {
        self.check_identity(pr)?;
        self.check_checkout_target(pr)?;
        ensure!(
            !self.commit_state("@")?.0,
            "Current jj @ has conflicts; resolve them before opening a PR"
        );
        let sparse = text(run(
            jj_command(&self.root).args(["sparse", "list"]),
            "inspect workspace sparsity",
        )?)?;
        ensure!(
            sparse.trim() == ".",
            "Opening a PR requires a full (non-sparse) jj working copy"
        );
        // A normal jj command snapshots edits and rejects stale workspaces. In particular,
        // do not use workspace update-stale, --ignore-working-copy, or --ignore-immutable.
        run(
            jj_workspace_command(&self.root).arg("status"),
            "snapshot current jj work (resolve conflicts, stale workspaces, or inaccessible files first)",
        )?;
        ensure!(
            !self.commit_state("@")?.0,
            "Current jj @ has conflicts; resolve them before opening a PR"
        );
        self.check_checkout_target(pr)?;
        let previous = self.checkout_commit()?;
        self.check_obstructions(&previous, &pr.head_sha)?;
        let result = (|| {
            ensure!(!self.commit_state("@")?.0, "Current jj @ has conflicts");
            self.check_obstructions(&previous, &pr.head_sha)?;
            self.check_ignore_changes(&previous, &pr.head_sha)?;
            let immutable = self.check_checkout_target(pr)?;
            let (action, operation) = if immutable {
                (
                    "new",
                    "create an empty child of the immutable PR head (stale workspaces are not overridden)",
                )
            } else {
                (
                    "edit",
                    "edit the exact PR head (immutable revisions and stale workspaces are not overridden)",
                )
            };
            run(
                jj_workspace_command(&self.root).args([action, &pr.head_sha]),
                operation,
            )?;
            self.ensure_checkout(pr)?;
            // Verify tracked files, but never put newly unignored local files into Git
            // objects, even if ignore rules or files changed concurrently with the guard.
            run(
                jj_workspace_command(&self.root).args([
                    "--config",
                    "snapshot.auto-track=\"none()\"",
                    "status",
                ]),
                "verify the checked-out working copy",
            )?;
            self.ensure_checkout(pr)?;
            Ok(previous.clone())
        })();
        result.with_context(|| {
            format!(
                "Previous work was snapshotted as {previous}. Return with: jj edit {previous} (or jj new {previous} if immutable); no rollback was attempted"
            )
        })
    }

    pub(crate) fn pinned_base_ref(&self, pr: &PullRequest) -> Result<String> {
        let response = self.get(&self.pr_endpoint(pr.number)?)?;
        let reviewer = self.authenticated_reviewer()?;
        let current = map_pr(&response, &self.target(), pr.number, &reviewer)?;
        ensure!(
            pr.same_revision(&current),
            "PR head, base, source, or reviewer changed; resolve the PR again"
        );
        let base_ref = required_string(&response["base"], "ref")?;
        validate_bookmark(base_ref)?;
        Ok(base_ref.to_owned())
    }

    fn commit_state(&self, revision: &str) -> Result<(bool, bool)> {
        let state = text(run(
            jj_command(&self.root).args([
                "log",
                "-r",
                revision,
                "--no-graph",
                "-T",
                "conflict ++ \"\\t\" ++ immutable",
            ]),
            "inspect the jj revision for conflicts and immutability",
        )?)?;
        match state.trim().split_once('\t') {
            Some((conflict @ ("true" | "false"), immutable @ ("true" | "false"))) => {
                Ok((conflict == "true", immutable == "true"))
            }
            _ => bail!("jj returned an unexpected revision state"),
        }
    }

    fn check_checkout_target(&self, pr: &PullRequest) -> Result<bool> {
        let (conflict, immutable) = self.commit_state(&pr.head_sha)?;
        ensure!(!conflict, "The PR head has jj conflicts; refusing checkout");
        Ok(immutable)
    }

    fn tree_paths(&self, sha: &str) -> Result<HashSet<PathBuf>> {
        validate_sha(sha)?;
        let bytes = run(
            self.git_command()
                .args(["ls-tree", "-r", "-z", "--full-tree", sha]),
            "inspect checkout paths",
        )?;
        let mut paths = HashSet::new();
        for entry in bytes.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let tab = entry
                .iter()
                .position(|b| *b == b'\t')
                .context("Invalid Git tree entry")?;
            let kind = std::str::from_utf8(&entry[..tab])?;
            ensure!(
                !kind.starts_with("160000 "),
                "Submodule checkouts are not supported; refusing to change this workspace"
            );
            let path =
                std::str::from_utf8(&entry[tab + 1..]).context("Checkout paths must be UTF-8")?;
            validate_path(path)?;
            ensure!(
                !path
                    .split('/')
                    .any(|part| part.eq_ignore_ascii_case(".git")
                        || part.eq_ignore_ascii_case(".jj")),
                "PR tree contains a reserved repository path"
            );
            paths.insert(PathBuf::from(path));
        }
        Ok(paths)
    }

    fn check_obstructions(&self, previous: &str, target: &str) -> Result<()> {
        let tracked = self.tree_paths(previous)?;
        for path in self.tree_paths(target)? {
            let mut relative = PathBuf::new();
            let parts: Vec<_> = path.components().collect();
            for (index, part) in parts.iter().enumerate() {
                relative.push(part);
                let full = self.root.join(&relative);
                let metadata = match fs::symlink_metadata(&full) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error).context("Cannot inspect a checkout path"),
                };
                if metadata.is_dir() {
                    if index + 1 == parts.len() {
                        check_tracked_directory(&self.root, &relative, &tracked)?;
                    }
                } else {
                    ensure!(
                        tracked.contains(&relative),
                        "Untracked or ignored file obstructs PR checkout: {}",
                        relative.display()
                    );
                    // jj removes a tracked file/symlink before replacing it with a directory.
                    break;
                }
            }
        }
        Ok(())
    }

    fn check_ignore_changes(&self, previous: &str, target: &str) -> Result<()> {
        let temporary = tempfile::Builder::new()
            .prefix("zed-pr-review-ignore-")
            .tempdir_in(&self.git_dir)?;
        let work_tree = temporary.path().join("worktree");
        fs::create_dir(&work_tree)?;
        // Git resolves a relative core.excludesFile from the work tree. Preserve that
        // meaning while checking another directory; info/exclude still uses our Git dir.
        let output = self
            .git_command()
            .args(["config", "--null", "--path", "--get", "core.excludesFile"])
            .stdin(Stdio::null())
            .output()
            .context("Cannot read Git's global ignore configuration")?;
        let excludes_file = match output.status.code() {
            Some(0) => {
                let value = text(output.stdout)?;
                let path = PathBuf::from(
                    value
                        .strip_suffix('\0')
                        .context("Invalid Git ignore configuration")?,
                );
                Some(if path.is_absolute() {
                    path
                } else {
                    self.root.join(path)
                })
            }
            Some(1) => None,
            _ => bail!("Cannot read Git's global ignore configuration"),
        };
        let check = IgnoreCheck {
            work_tree,
            tracked: self.tree_paths(previous)?,
            target: self.tree_paths(target)?,
            excludes_file,
        };
        self.check_external_ignores(&check, previous, target)?;
        for path in &check.target {
            if path.file_name().is_none_or(|name| name != ".gitignore") {
                continue;
            }
            let path_text = path.to_str().context("Ignore paths must be UTF-8")?;
            let entry = run(
                self.git_command()
                    .args(["ls-tree", "-z", target, "--", path_text]),
                "inspect target ignore file type",
            )?;
            ensure!(
                entry.starts_with(b"100644 blob ") || entry.starts_with(b"100755 blob "),
                "Non-regular target .gitignore files are not supported for safe checkout"
            );
            let destination = check.work_tree.join(path);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::write(destination, self.blob(target, path_text)?)?;
        }
        // The backing Git index need not describe jj @ (especially when non-colocated).
        // Populate only a private index from the already-snapshotted commit. No -u, no
        // working-copy reads, and no private file contents are added to the object store.
        let index = temporary.path().join("index");
        run(
            self.ignore_command(&self.root, &check)
                .env("GIT_INDEX_FILE", &index)
                .args(["read-tree", previous]),
            "prepare a private index for checking ignored paths",
        )?;
        let ignored = run(
            self.ignore_command(&self.root, &check)
                .env("GIT_INDEX_FILE", &index)
                .args([
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                    "-z",
                    "--",
                    ".",
                    ":(exclude).jj",
                    ":(exclude).git",
                ]),
            "list previously ignored local paths",
        )?;
        let paths = ignored
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                let path = std::str::from_utf8(path)
                    .context("Ignored paths must be UTF-8 for safe checkout")?
                    .trim_end_matches('/');
                validate_path(path)?;
                Ok(PathBuf::from(path))
            })
            .collect::<Result<Vec<_>>>()?;
        self.check_ignored_paths(&check, paths)
    }

    fn check_external_ignores(
        &self,
        check: &IgnoreCheck,
        previous: &str,
        target: &str,
    ) -> Result<()> {
        let info = text(run(
            self.git_command()
                .args(["rev-parse", "--git-path", "info/exclude"]),
            "locate repository ignore rules",
        )?)?;
        let mut paths = vec![self.root.join(info.trim_end_matches(['\r', '\n']))];
        let global = check.excludes_file.clone().or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
                .map(|config| config.join("git/ignore"))
        });
        paths.extend(global);
        for path in paths {
            let resolved = match path.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("Cannot resolve an external ignore file"),
            };
            if let Ok(relative) = resolved.strip_prefix(&self.root)
                && check.tracked.contains(relative)
            {
                // Global/info excludes can point back into this checkout. Do not
                // assume their current contents survive if the PR changes them.
                let name = relative.to_str().context("Ignore paths must be UTF-8")?;
                let before = run(
                    self.git_command()
                        .args(["ls-tree", "-z", previous, "--", name]),
                    "inspect current external ignore file",
                )?;
                let after = run(
                    self.git_command()
                        .args(["ls-tree", "-z", target, "--", name]),
                    "inspect target external ignore file",
                )?;
                ensure!(
                    before == after,
                    "A repository/global ignore file changes at the PR head; refusing checkout: {}",
                    relative.display()
                );
            }
        }
        Ok(())
    }

    fn ignore_command(&self, work_tree: &Path, check: &IgnoreCheck) -> Command {
        let mut command = self.git_command();
        command
            .current_dir(work_tree)
            .arg("--work-tree")
            .arg(work_tree)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "submodule.recurse=false",
            ]);
        if let Some(path) = &check.excludes_file {
            let mut setting = std::ffi::OsString::from("core.excludesFile=");
            setting.push(path);
            command.arg("-c").arg(setting);
        }
        command
    }

    fn ignored_paths(
        &self,
        work_tree: &Path,
        check: &IgnoreCheck,
        paths: &[String],
    ) -> Result<HashSet<String>> {
        if paths.is_empty() {
            return Ok(HashSet::new());
        }
        // A file avoids pipe deadlocks when both the input and Git's output exceed
        // the pipe buffer. NUL delimiters preserve spaces and newlines in filenames.
        let mut input = tempfile::tempfile_in(&self.git_dir)?;
        for path in paths {
            input.write_all(path.as_bytes())?;
            input.write_all(b"\0")?;
        }
        input.rewind()?;
        let output = self
            .ignore_command(work_tree, check)
            .args(["check-ignore", "--no-index", "-z", "--stdin"])
            .stdin(input)
            .output()
            .context("Cannot check local ignore rules")?;
        ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "Cannot check local ignore rules safely"
        );
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                Ok(std::str::from_utf8(path)
                    .context("Git returned a non-UTF-8 ignored path")?
                    .to_owned())
            })
            .collect()
    }

    fn check_ignored_paths(&self, check: &IgnoreCheck, mut pending: Vec<PathBuf>) -> Result<()> {
        let mut checked_ancestors = HashSet::new();
        while !pending.is_empty() {
            let mut candidates = Vec::new();
            for path in pending {
                if check.tracked.contains(&path) {
                    continue;
                }
                // Untracked .gitignore files survive the switch. Copy each once, before
                // querying the batch, including review state's retained '*' rule.
                for ancestor in path.ancestors().skip(1) {
                    if !checked_ancestors.insert(ancestor.to_path_buf()) {
                        continue;
                    }
                    let ignore = ancestor.join(".gitignore");
                    if check.tracked.contains(&ignore) || check.target.contains(&ignore) {
                        continue;
                    }
                    match fs::symlink_metadata(self.root.join(&ignore)) {
                        Ok(metadata) if metadata.is_file() => {
                            let destination = check.work_tree.join(&ignore);
                            fs::create_dir_all(destination.parent().unwrap())?;
                            fs::write(destination, fs::read(self.root.join(ignore))?)?;
                        }
                        Ok(metadata) if metadata.is_symlink() => {} // Git does not follow these.
                        Ok(_) => bail!(
                            "Non-regular local .gitignore files are not supported for safe checkout"
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error).context("Cannot inspect a retained ignore file");
                        }
                    }
                }
                let metadata = fs::symlink_metadata(self.root.join(&path))
                    .context("An ignored local path changed while checking checkout safety")?;
                let path_text = path
                    .to_str()
                    .context("Ignored paths must be UTF-8 for safe checkout")?;
                validate_path(path_text)?;
                let query = if metadata.is_dir() {
                    format!("{path_text}/")
                } else {
                    path_text.to_owned()
                };
                candidates.push((path, query, metadata.is_dir()));
            }
            let queries: Vec<_> = candidates
                .iter()
                .map(|(_, query, _)| query.clone())
                .collect();
            let ignored = self.ignored_paths(&check.work_tree, check, &queries)?;
            pending = Vec::new();
            let mut exposed = Vec::new();
            for (path, query, directory) in candidates {
                if ignored.contains(&query) {
                    continue;
                }
                if directory {
                    // Descend only when the target does not ignore the entire directory.
                    // Process all siblings together rather than spawning Git per path.
                    for entry in fs::read_dir(self.root.join(&path))? {
                        pending.push(path.join(entry?.file_name()));
                    }
                } else {
                    exposed.push(query);
                }
            }
            let previously_ignored = self.ignored_paths(&self.root, check, &exposed)?;
            for path in exposed {
                ensure!(
                    !previously_ignored.contains(&path),
                    "Local path would no longer be ignored at the PR head: {}; keep it ignored or move it outside the workspace before opening the PR",
                    path
                );
            }
        }
        Ok(())
    }

    fn check_transport_url(&self, repo: &str, url: &str) -> Result<()> {
        // Reject URL rewrites rather than letting local configuration redirect credentials
        // or turn an HTTPS fetch into a local/SSH/external-helper operation.
        let actual_url = text(run(
            self.fetch_command_for(repo)
                .args(["ls-remote", "--get-url", url]),
            "check the GitHub transport URL",
        )?)?;
        ensure!(
            actual_url.trim() == url,
            "Git URL rewriting is not supported for PR fetches"
        );
        Ok(())
    }

    fn pr_fetch_command_for(&self, repo: &str) -> Command {
        let mut command = self.fetch_command_for(repo);
        command.args([
            "fetch",
            "--atomic",
            "--no-tags",
            "--no-prune",
            "--no-prune-tags",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--no-write-commit-graph",
            "--no-write-fetch-head",
            "--refmap=",
            "--no-filter",
        ]);
        command
    }

    fn fetch_command_for(&self, repo: &str) -> Command {
        let mut command = self.git_command();
        command
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "credential.helper=",
                "-c",
                &format!(
                    "credential.https://{}.helper=!gh auth git-credential",
                    self.host
                ),
                "-c",
                "credential.interactive=false",
                "-c",
                "http.followRedirects=false",
                "-c",
                &format!(
                    "http.https://{}/{}.git.followRedirects=false",
                    self.host, repo
                ),
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ])
            .env("GIT_ALLOW_PROTOCOL", "https")
            .env("GH_HOST", &self.host)
            .env("GH_PROMPT_DISABLED", "1")
            // GIT_TERMINAL_PROMPT=0 does not disable askpass. An explicit empty
            // value also prevents fallback to core.askPass or SSH_ASKPASS.
            .env("GIT_ASKPASS", "")
            .env_remove("GH_REPO")
            .env_remove("GH_DEBUG");
        command
    }

    pub fn files(&self, number: u64) -> Result<Vec<ChangedFile>> {
        let files = self.get_pages::<ChangedFile>(&format!(
            "{}/files?per_page=100",
            self.pr_endpoint(number)?
        ))?;
        check_file_count(files.len())?;
        for file in &files {
            validate_path(&file.filename)?;
            if let Some(previous) = &file.previous_filename {
                validate_path(previous)?;
            }
        }
        Ok(files)
    }

    /// Read raw blob bytes. cat-file does not apply textconv or working-tree filters.
    pub fn blob(&self, sha: &str, path: &str) -> Result<Vec<u8>> {
        validate_sha(sha)?;
        validate_path(path)?;
        run(
            self.git_command()
                .args(["cat-file", "blob", &format!("{sha}:{path}")]),
            "read local Git blob (objects must already be present; fetch explicitly if needed)",
        )
    }

    pub fn merge_base(&self, pr: &PullRequest) -> Result<String> {
        self.check_identity(pr)?;
        for (label, sha) in [("base", &pr.base_sha), ("head", &pr.head_sha)] {
            let kind = run(self.git_command().args(["cat-file", "-t", sha]), "inspect local Git commit")
                .with_context(|| format!("Pinned PR {label} commit {sha} is unavailable locally; fetch the corresponding branch explicitly with jj git fetch, then retry. No fetch was attempted"))?;
            ensure!(
                text(kind)?.trim() == "commit",
                "Pinned PR {label} SHA is not a commit"
            );
        }
        let output = text(run(
            self.git_command()
                .args(["merge-base", "--all", &pr.head_sha, &pr.base_sha]),
            "find a local merge base (fetch missing history explicitly with jj git fetch, then retry)",
        )?)?;
        let bases: Vec<_> = output.lines().collect();
        ensure!(
            bases.len() == 1,
            "Expected one merge base for the pinned PR commits; found {}",
            bases.len()
        );
        validate_sha(bases[0])?;
        Ok(bases[0].to_ascii_lowercase())
    }

    pub fn pending_review(&self, number: u64) -> Result<bool> {
        validate_login(&self.reviewer)?;
        has_pending_review(&self.reviews(number)?, &self.reviewer)
    }

    /// Send exactly one POST. The caller must record uncertainty *before* this call.
    /// `payload.commit_id` must name the pinned head. Errors must be reconciled, not retried.
    pub fn submit(&self, pr: &PullRequest, payload: &Value) -> Result<Value> {
        self.check_identity(pr)?;
        ensure!(
            payload
                .get("commit_id")
                .and_then(Value::as_str)
                .is_some_and(|sha| sha.eq_ignore_ascii_case(&pr.head_sha)),
            "Review payload commit_id must equal the pinned PR head"
        );
        let endpoint = format!("{}/reviews", self.pr_endpoint(pr.number)?);
        self.post_json(&endpoint, payload)
    }

    fn post_json(&self, endpoint: &str, payload: &Value) -> Result<Value> {
        self.send_json("POST", endpoint, payload)
    }

    fn send_json(&self, method: &str, endpoint: &str, payload: &Value) -> Result<Value> {
        let body = serde_json::to_vec(payload).context("Cannot encode GitHub request")?;
        let mut command = self.api_command(method, endpoint)?;
        command.args(["--input", "-"]).stdin(Stdio::piped());
        let mut child = command
            .spawn()
            .context("Cannot start gh mutation; no retry was attempted")?;
        let write_result = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(&body),
            None => Err(std::io::Error::other("gh stdin was not piped")),
        };
        // Always reap the child, even if writing its stdin failed. Never log the body or stderr.
        let output = child.wait_with_output().context(
            "Cannot collect GitHub response; outcome is uncertain, check GitHub before retrying",
        )?;
        write_result.context(
            "Cannot write GitHub request; outcome is uncertain, check GitHub before retrying",
        )?;
        let bytes = successful_output(
            output,
            "complete GitHub mutation; outcome may be uncertain, check GitHub before retrying (no retry was attempted)",
        )?;
        serde_json::from_slice(&bytes).context(
            "Invalid GitHub response JSON; outcome is uncertain, check GitHub before retrying",
        )
    }

    pub fn reviews(&self, number: u64) -> Result<Vec<Value>> {
        self.get_pages(&format!(
            "{}/reviews?per_page=100",
            self.pr_endpoint(number)?
        ))
    }

    pub fn review_comments(&self, number: u64, id: u64) -> Result<Vec<Value>> {
        ensure!(id > 0, "Review ID must be positive");
        self.get_pages(&format!(
            "{}/reviews/{id}/comments?per_page=100",
            self.pr_endpoint(number)?
        ))
    }

    fn target(&self) -> Target {
        Target {
            host: self.host.clone(),
            repo: self.repo.clone(),
        }
    }

    fn repo_argument(&self) -> String {
        format!("{}/{}", self.host, self.repo)
    }

    fn pr_endpoint(&self, number: u64) -> Result<String> {
        self.validate_target()?;
        ensure!(number > 0, "PR number must be positive");
        Ok(format!("repos/{}/pulls/{number}", self.repo))
    }

    fn validate_target(&self) -> Result<()> {
        validate_host(&self.host)?;
        validate_repo(&self.repo)
    }

    /// Validate optional selectors against a pinned tab without discovery or any I/O.
    /// Explicit arguments may confirm the registered PR, but must never rebind it.
    pub fn check_explicit_pr(
        &self,
        pr: &PullRequest,
        requested_pr: Option<&str>,
        requested_repo: Option<&str>,
    ) -> Result<()> {
        self.check_identity(pr)?;
        let requested_pr = requested_pr.map(parse_pr_spec).transpose()?;
        let requested_repo = requested_repo.map(parse_repo_spec).transpose()?;
        let target = explicit_target(requested_pr.as_ref(), requested_repo.as_ref())?;
        ensure!(
            requested_pr
                .as_ref()
                .is_none_or(|requested| requested.number == pr.number),
            "--pr number does not match the registered PR; open the requested PR separately"
        );
        if let Some(target) = target {
            ensure!(
                target.host.eq_ignore_ascii_case(&pr.host)
                    && target.repo.eq_ignore_ascii_case(&pr.repo),
                "--pr/--repo target does not match the registered PR; open the requested PR separately"
            );
        }
        Ok(())
    }

    pub(crate) fn check_identity(&self, pr: &PullRequest) -> Result<()> {
        self.validate_target()?;
        ensure!(pr.number > 0, "PR number must be positive");
        ensure!(
            self.host.eq_ignore_ascii_case(&pr.host) && self.repo.eq_ignore_ascii_case(&pr.repo),
            "PR host/repository does not match this client"
        );
        validate_login(&self.reviewer)?;
        ensure!(
            self.reviewer.eq_ignore_ascii_case(&pr.reviewer),
            "PR reviewer does not match this client"
        );
        validate_sha(&pr.head_sha)?;
        validate_sha(&pr.base_sha)?;
        Ok(())
    }

    fn git_command(&self) -> Command {
        let mut command = Command::new("git");
        clear_git_context(&mut command);
        command
            .current_dir(&self.root)
            .args(["--no-pager", "--git-dir"])
            .arg(&self.git_dir)
            // Avoid replacement refs and implicit network writes in partial clones.
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn api_command(&self, method: &str, endpoint: &str) -> Result<Command> {
        self.validate_target()?;
        let mut command = gh_command(&self.root, Some(&self.host));
        command.args([
            "api",
            "--hostname",
            &self.host,
            "--method",
            method,
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            endpoint,
        ]);
        Ok(command)
    }

    pub(crate) fn get(&self, endpoint: &str) -> Result<Value> {
        let bytes = run(
            &mut self.api_command("GET", endpoint)?,
            "read GitHub API (check gh authentication and repository access)",
        )?;
        serde_json::from_slice(&bytes).context("Invalid GitHub API JSON")
    }

    pub(crate) fn get_pages<T: DeserializeOwned>(&self, endpoint: &str) -> Result<Vec<T>> {
        let bytes = run(
            self.api_command("GET", endpoint)?
                .args(["--paginate", "--slurp"]),
            "read paginated GitHub API (check gh authentication and repository access)",
        )?;
        decode_pages(&bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    host: String,
    repo: String,
}

#[derive(Debug)]
struct RepoSpec {
    host: Option<String>,
    repo: String,
}

#[derive(Debug)]
struct PrSpec {
    target: Option<Target>,
    number: u64,
}

fn validate_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label.as_bytes()[0].is_ascii_alphanumeric()
                    && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }),
        "Invalid GitHub hostname; use a DNS hostname without credentials, port, or path"
    );
    Ok(())
}

fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    ensure!(parts.len() == 2, "Repository must be owner/repo");
    let owner = parts[0];
    ensure!(
        !owner.is_empty()
            && owner.as_bytes()[0].is_ascii_alphanumeric()
            && owner.as_bytes()[owner.len() - 1].is_ascii_alphanumeric()
            && owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid repository owner"
    );
    let name = parts[1];
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.starts_with('-')
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
        "Invalid repository name"
    );
    Ok(())
}

pub(crate) fn reviewer_payload(repo: &str, reviewer: &str) -> Result<Value> {
    validate_requested_reviewer(reviewer)?;
    if let Some((owner, team)) = reviewer.split_once('/') {
        ensure!(
            repo.split_once('/')
                .is_some_and(|(repo_owner, _)| repo_owner.eq_ignore_ascii_case(owner)),
            "Reviewer team must belong to the base repository's organization"
        );
        Ok(json!({"team_reviewers": [team]}))
    } else {
        Ok(json!({"reviewers": [reviewer]}))
    }
}

pub(crate) fn validate_requested_reviewer(reviewer: &str) -> Result<()> {
    if let Some((owner, team)) = reviewer.split_once('/') {
        validate_login(owner).context("Invalid reviewer organization")?;
        ensure!(
            !team.is_empty()
                && team.as_bytes()[0].is_ascii_alphanumeric()
                && team
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
            "Invalid reviewer team; use organization/team-slug"
        );
    } else {
        validate_login(reviewer)
            .context("Invalid reviewer; use a login or organization/team-slug")?;
    }
    Ok(())
}

fn validate_login(login: &str) -> Result<()> {
    let name = login.strip_suffix("[bot]").unwrap_or(login);
    ensure!(
        !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid GitHub login"
    );
    Ok(())
}

fn validate_sha(sha: &str) -> Result<()> {
    ensure!(
        matches!(sha.len(), 40 | 64) && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "Expected a full hexadecimal Git object ID (40 or 64 characters)"
    );
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\0', '\\'])
            && !path.starts_with('/')
            && !path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."),
        "Expected a repository-relative file path without traversal"
    );
    Ok(())
}

fn validate_bookmark(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "@"
            && !name.starts_with('-')
            && !name.ends_with('.')
            && !name.contains("..")
            && !name.contains("@{")
            && !name
                .bytes()
                .any(|c| c <= b' ' || c == 127 || b"~^:?*[\\".contains(&c))
            && !name
                .split('/')
                .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock")),
        "Invalid local bookmark name"
    );
    Ok(())
}

fn parse_repo_spec(value: &str) -> Result<RepoSpec> {
    let parts: Vec<_> = value.split('/').collect();
    let (host, repo) = match parts.as_slice() {
        [_, _] => (None, value.to_owned()),
        [host, owner, name] => {
            validate_host(host)?;
            (Some(host.to_ascii_lowercase()), format!("{owner}/{name}"))
        }
        _ => bail!("--repo must be [HOST/]owner/repo"),
    };
    validate_repo(&repo)?;
    Ok(RepoSpec { host, repo })
}

fn parse_number(value: &str) -> Result<u64> {
    ensure!(
        !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()),
        "PR number must contain only decimal digits"
    );
    let number = value.parse::<u64>().context("PR number is too large")?;
    ensure!(number > 0, "PR number must be positive");
    Ok(number)
}

fn parse_pr_spec(value: &str) -> Result<PrSpec> {
    if let Some(url) = value.strip_prefix("https://") {
        let parts: Vec<_> = url.strip_suffix('/').unwrap_or(url).split('/').collect();
        ensure!(
            parts.len() == 5 && parts[3] == "pull",
            "PR URL must be https://HOST/owner/repo/pull/N"
        );
        validate_host(parts[0])?;
        let repo = format!("{}/{}", parts[1], parts[2]);
        validate_repo(&repo)?;
        Ok(PrSpec {
            target: Some(Target {
                host: parts[0].to_ascii_lowercase(),
                repo,
            }),
            number: parse_number(parts[4])?,
        })
    } else {
        Ok(PrSpec {
            target: None,
            number: parse_number(value)?,
        })
    }
}

fn source_url_matches(url: &str, pr: &PullRequest) -> bool {
    parse_remote_url(url).is_ok_and(|target| {
        target.host.eq_ignore_ascii_case(&pr.host)
            && target.repo.eq_ignore_ascii_case(&pr.head_repo)
    })
}

/// Accept common HTTPS, SSH, and scp-style Git remote URLs, but not local paths.
fn parse_remote_url(value: &str) -> Result<Target> {
    let (host, path) = if let Some(rest) = value.strip_prefix("https://") {
        rest.split_once('/')
            .context("Remote URL has no repository path")?
    } else if let Some(rest) = value.strip_prefix("ssh://git@") {
        rest.split_once('/')
            .context("SSH remote URL has no repository path")?
    } else if let Some(rest) = value.strip_prefix("git@") {
        rest.split_once(':')
            .context("SSH remote URL has no repository path")?
    } else {
        bail!("Expected an HTTPS or git SSH remote URL");
    };
    validate_host(host)?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let repo = path.strip_suffix(".git").unwrap_or(path);
    validate_repo(repo)?;
    Ok(Target {
        host: host.to_ascii_lowercase(),
        repo: repo.to_owned(),
    })
}

fn explicit_target(pr: Option<&PrSpec>, repo: Option<&RepoSpec>) -> Result<Option<Target>> {
    if let Some(target) = pr.and_then(|pr| pr.target.as_ref()) {
        if let Some(repo) = repo {
            ensure!(
                repo.repo.eq_ignore_ascii_case(&target.repo)
                    && repo
                        .host
                        .as_ref()
                        .is_none_or(|host| host.eq_ignore_ascii_case(&target.host)),
                "--pr URL conflicts with --repo"
            );
        }
        return Ok(Some(target.clone()));
    }
    Ok(repo.map(|repo| Target {
        host: repo.host.clone().unwrap_or_else(|| "github.com".to_owned()),
        repo: repo.repo.clone(),
    }))
}

fn map_repo_view(value: &Value) -> Result<Target> {
    let target = parse_remote_url(required_string(value, "url")?)?;
    let name = required_string(value, "nameWithOwner")?;
    validate_repo(name)?;
    ensure!(
        target.repo.eq_ignore_ascii_case(name),
        "gh repo view returned inconsistent repository identity"
    );
    Ok(Target {
        repo: name.to_owned(),
        ..target
    })
}

fn select_discovery_bookmark(
    revision: &RecordedRevision,
    mut list: impl FnMut(&str) -> Result<String>,
) -> Result<(String, String)> {
    for sha in std::iter::once(revision.commit_id.as_str()).chain(revision.empty_parent.as_deref())
    {
        if let Some(bookmark) = select_bookmark(&list(sha)?)? {
            return Ok((sha.to_owned(), bookmark));
        }
    }
    bail!(
        "No local bookmark points at captured jj @ or its eligible empty-child parent; supply --pr explicitly (no other ancestors are considered)"
    )
}

fn select_bookmark(output: &str) -> Result<Option<String>> {
    let mut names = Vec::new();
    for line in output.lines() {
        let (name, conflict) = line
            .split_once('\t')
            .context("Invalid jj bookmark output")?;
        validate_bookmark(name)?;
        match conflict {
            "false" => names.push(name),
            "true" => bail!(
                "A local bookmark at the lookup revision is conflicted; resolve it or supply --pr explicitly"
            ),
            _ => bail!("Invalid jj bookmark conflict flag"),
        }
    }
    ensure!(
        names.len() <= 1,
        "Multiple local bookmarks point at the lookup revision; supply --pr explicitly"
    );
    Ok(names.first().map(|name| (*name).to_owned()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedPr {
    number: u64,
    head_ref_oid: String,
    url: String,
    head_repository: Option<ListedRepo>,
    head_repository_owner: Option<ListedOwner>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedRepo {
    name: String,
    name_with_owner: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListedOwner {
    login: String,
}

struct SelectedPr {
    number: u64,
    head_repo: String,
}

fn select_pr(candidates: Vec<ListedPr>, sha: &str, target: &Target) -> Result<SelectedPr> {
    ensure!(
        candidates.len() < 100,
        "PR lookup reached its 100-result limit; supply --pr explicitly to avoid an incomplete search"
    );
    let mut matching = Vec::new();
    for candidate in candidates {
        validate_sha(&candidate.head_ref_oid)?;
        if candidate.head_ref_oid.eq_ignore_ascii_case(sha) {
            matching.push(candidate);
        }
    }
    ensure!(
        !matching.is_empty(),
        "No open PR for the local bookmark has exactly the lookup commit; check the checkout or supply --pr/--repo explicitly"
    );
    ensure!(
        matching.len() == 1,
        "Multiple open PRs match the local bookmark and lookup commit; supply --pr explicitly"
    );
    let candidate = matching.pop().context("Missing selected PR")?;
    check_pr_url(&candidate.url, target, candidate.number)?;
    let repository = candidate
        .head_repository
        .context("PR head repository was deleted or is unavailable")?;
    let owner = candidate
        .head_repository_owner
        .context("PR head repository owner is unavailable")?;
    let head_repo = format!("{}/{}", owner.login, repository.name);
    validate_repo(&head_repo)?;
    if let Some(full_name) = repository.name_with_owner {
        ensure!(
            head_repo.eq_ignore_ascii_case(&full_name),
            "PR list returned inconsistent head repository identity"
        );
    }
    Ok(SelectedPr {
        number: candidate.number,
        head_repo,
    })
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("GitHub response is missing string field {key}"))
}

fn check_pr_url(url: &str, target: &Target, number: u64) -> Result<()> {
    let spec = parse_pr_spec(url)?;
    let actual = spec
        .target
        .context("GitHub PR response did not include an HTTPS URL")?;
    ensure!(
        spec.number == number
            && actual.host.eq_ignore_ascii_case(&target.host)
            && actual.repo.eq_ignore_ascii_case(&target.repo),
        "GitHub response does not match the requested PR identity"
    );
    Ok(())
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn map_branch_pr(value: &Value, target: &Target) -> Result<BranchPr> {
    let number = value
        .get("number")
        .and_then(Value::as_u64)
        .context("GitHub PR response is missing numeric field number")?;
    ensure!(number > 0, "GitHub PR number must be positive");
    let url = required_string(value, "html_url")?;
    check_pr_url(url, target, number)?;
    let state_str = required_string(value, "state")?;
    let merged_at = value
        .get("merged_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let state = if merged_at.is_some() {
        BranchPrState::Merged
    } else if state_str == "open" {
        BranchPrState::Open
    } else {
        BranchPrState::Closed
    };
    let head_ref = required_string(&value["head"], "ref")?;
    let head_sha = required_string(&value["head"], "sha")?;
    validate_sha(head_sha)?;
    let base_ref = required_string(&value["base"], "ref")?;
    let title = required_string(value, "title")?;
    let author = value
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let draft = value.get("draft").and_then(Value::as_bool).unwrap_or(false);
    let node_id = value
        .get("node_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok(BranchPr {
        number,
        node_id,
        state,
        draft,
        title: title.to_owned(),
        head_ref: head_ref.to_owned(),
        head_sha: head_sha.to_ascii_lowercase(),
        base_ref: base_ref.to_owned(),
        url: url.to_owned(),
        author: author.to_owned(),
    })
}

fn map_pr(value: &Value, target: &Target, number: u64, reviewer: &str) -> Result<PullRequest> {
    validate_host(&target.host)?;
    validate_repo(&target.repo)?;
    validate_login(reviewer)?;
    ensure!(
        number > 0 && value.get("number").and_then(Value::as_u64) == Some(number),
        "GitHub returned a different PR number"
    );
    ensure!(
        required_string(value, "state")? == "open"
            && value.get("merged").and_then(Value::as_bool) == Some(false)
            && value.get("merged_at").is_none_or(Value::is_null),
        "PR must be open and not merged"
    );
    let url = required_string(value, "html_url")?;
    check_pr_url(url, target, number)?;
    let head = value.get("head").context("PR response is missing head")?;
    let base = value.get("base").context("PR response is missing base")?;
    let head_sha = required_string(head, "sha")?;
    let base_sha = required_string(base, "sha")?;
    validate_sha(head_sha)?;
    validate_sha(base_sha)?;
    let head_repo = required_string(&head["repo"], "full_name")
        .context("PR head repository was deleted or is unavailable")?;
    validate_repo(head_repo)?;
    let base_repo = required_string(&base["repo"], "full_name")?;
    ensure!(
        base_repo.eq_ignore_ascii_case(&target.repo),
        "PR base repository does not match this client"
    );
    let head_ref = required_string(head, "ref")?;
    validate_bookmark(head_ref)?;
    let author = required_string(&value["user"], "login")?;
    validate_login(author)?;
    Ok(PullRequest {
        host: target.host.to_ascii_lowercase(),
        repo: target.repo.clone(),
        number,
        url: url.to_owned(),
        title: required_string(value, "title")?.to_owned(),
        head_sha: head_sha.to_ascii_lowercase(),
        base_sha: base_sha.to_ascii_lowercase(),
        head_repo: head_repo.to_owned(),
        head_ref: head_ref.to_owned(),
        author: author.to_owned(),
        reviewer: reviewer.to_owned(),
    })
}

fn check_file_count(count: usize) -> Result<()> {
    ensure!(
        count < 3000,
        "GitHub's 3000-file limit was reached; the PR file list may be incomplete, so this PR cannot be reviewed safely"
    );
    Ok(())
}

fn decode_pages<T: DeserializeOwned>(bytes: &[u8]) -> Result<Vec<T>> {
    let pages: Vec<Vec<T>> = serde_json::from_slice(bytes)
        .context("Expected arrays of pages from gh api --paginate --slurp")?;
    Ok(pages.into_iter().flatten().collect())
}

fn has_pending_review(reviews: &[Value], reviewer: &str) -> Result<bool> {
    let mut pending = false;
    for review in reviews {
        let state = required_string(review, "state")?;
        let user = review
            .get("user")
            .context("Review response is missing user")?;
        // GitHub may return null for a deleted account, which cannot be our reviewer.
        if !user.is_null() {
            let login = required_string(user, "login")?;
            pending |= state == "PENDING" && login.eq_ignore_ascii_case(reviewer);
        }
    }
    Ok(pending)
}

fn clear_git_context(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
}

/// Resolve the workspace containing `directory` without snapshotting current work.
pub fn workspace_root(directory: &Path) -> Result<PathBuf> {
    let directory = directory
        .canonicalize()
        .context("Cannot resolve review directory")?;
    command_path(
        run(jj_command(&directory).arg("root"), "resolve jj root")?,
        &directory,
    )
}

/// Resolve the Git object store without importing refs or changing the working copy.
pub fn workspace_git_dir(root: &Path) -> Result<PathBuf> {
    command_path(
        run(
            jj_command(root).args(["git", "root"]),
            "resolve jj Git directory",
        )?,
        root,
    )
}

// The random directory reserves a namespace for this operation, even across processes.
// Refs/bookmarks intentionally outlive it, so both PR objects and previous work remain reachable.
fn transport_token(git_dir: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("zed-pr-review-")
        .rand_bytes(16)
        .tempdir_in(git_dir)
        .context("Cannot reserve a unique namespace in jj's Git directory")
}

fn check_tracked_directory(root: &Path, relative: &Path, tracked: &HashSet<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            check_tracked_directory(root, &path, tracked)?;
        } else {
            ensure!(
                tracked.contains(&path),
                "Untracked or ignored file obstructs PR checkout: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn jj_workspace_command(directory: &Path) -> Command {
    let mut command = Command::new("jj");
    clear_git_context(&mut command);
    command.current_dir(directory).env("JJ_PAGER", "cat").args([
        "--no-pager",
        "--color=never",
        "--config",
        "snapshot.auto-update-stale=false",
        "--config",
        "git.abandon-unreachable-commits=false",
    ]);
    command
}

fn jj_command(directory: &Path) -> Command {
    let mut command = Command::new("jj");
    clear_git_context(&mut command);
    command.current_dir(directory).env("JJ_PAGER", "cat").args([
        "--ignore-working-copy",
        "--no-pager",
        "--color=never",
    ]);
    command
}

fn gh_command(directory: &Path, host: Option<&str>) -> Command {
    let mut command = Command::new("gh");
    clear_git_context(&mut command);
    command
        .current_dir(directory)
        .env_remove("GH_REPO")
        .env_remove("GH_DEBUG")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(host) = host {
        command.env("GH_HOST", host);
    }
    command
}

/// Capture the recorded working-copy commit without snapshotting or importing refs.
pub fn checkout_sha(root: &Path) -> Result<String> {
    let sha = text(run(
        jj_command(root).args(["log", "-r", "@", "--no-graph", "-T", "commit_id"]),
        "read the exact jj @ commit",
    )?)?;
    let sha = sha.trim();
    validate_sha(sha)?;
    Ok(sha.to_ascii_lowercase())
}

fn command_path(bytes: Vec<u8>, relative_to: &Path) -> Result<PathBuf> {
    let path = text(bytes)?;
    let path = path.trim_end_matches(['\r', '\n']);
    ensure!(
        !path.is_empty() && !path.contains(['\r', '\n', '\0']),
        "Command returned an invalid repository path"
    );
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        relative_to.join(path)
    };
    let path = path
        .canonicalize()
        .context("Cannot resolve repository path")?;
    ensure!(path.is_dir(), "Repository path is not a directory");
    Ok(path)
}

fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).context("Command output was not UTF-8")
}

fn background_timeout_for(command: &Command) -> Option<Duration> {
    let timeout = BACKGROUND_COMMAND_TIMEOUT.get()?;
    let program = Path::new(command.get_program()).file_name()?;
    // Local Git/jj inspection remains on the blocking fast path. Only gh and Git
    // fetch can perform network transport through this shared runner.
    let network = program == "gh"
        || program == "gh.exe"
        || ((program == "git" || program == "git.exe")
            && command.get_args().any(|arg| arg == "fetch"));
    network.then_some(timeout)
}

fn stop_background_child(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child is the leader of a fresh process group. Kill its transport and
        // credential helpers too; killing only Git/gh can leave those running.
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // Also cover platforms without process groups and failures of the group kill.
    let _ = child.kill();
    let _ = child.wait();
}

fn run_background(command: &mut Command, operation: &str, timeout: Duration) -> Result<Vec<u8>> {
    // Anonymous files avoid pipe-buffer deadlocks and waiting for EOF from helpers
    // that inherited stdout/stderr. Never include their contents in command errors.
    let mut stdout = tempfile::tempfile().context("Cannot capture background command output")?;
    let stderr = tempfile::tempfile().context("Cannot capture background command errors")?;
    command
        .stdin(Stdio::null())
        .stdout(
            stdout
                .try_clone()
                .context("Cannot capture background command output")?,
        )
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let start = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot {operation}; is the required executable installed?"))?;
    let mut interval = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                stop_background_child(&mut child);
                return Err(error).with_context(|| format!("Cannot wait to {operation}"));
            }
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            stop_background_child(&mut child);
            bail!("Cannot {operation}: background command timed out after {timeout:?}");
        }
        std::thread::sleep(interval.min(remaining));
        interval = (interval * 2).min(Duration::from_millis(100));
    };
    // Check status before reading potentially large output from a failed command.
    successful_output(
        Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
        operation,
    )?;
    stdout
        .rewind()
        .context("Cannot read background command output")?;
    let mut bytes = Vec::new();
    stdout
        .read_to_end(&mut bytes)
        .context("Cannot read background command output")?;
    Ok(bytes)
}

fn run(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
    if let Some(timeout) = background_timeout_for(command) {
        return run_background(command, operation, timeout);
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Cannot {operation}; is the required executable installed?"))?;
    successful_output(output, operation)
}

fn successful_output(output: Output, operation: &str) -> Result<Vec<u8>> {
    // gh stderr/debug output can contain authentication data; do not attach it to errors.
    ensure!(
        output.status.success(),
        "Cannot {operation} (exit status {})",
        output.status
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const BASE: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn target() -> Target {
        Target {
            host: "github.com".into(),
            repo: "upstream/project".into(),
        }
    }

    fn client() -> Client {
        Client {
            root: PathBuf::from("/unused/workspace"),
            git_dir: PathBuf::from("/unused/git"),
            host: target().host,
            repo: target().repo,
            reviewer: "reviewer".into(),
        }
    }

    fn pull_json() -> Value {
        json!({
            "number": 42, "state": "open", "merged": false, "merged_at": null,
            "html_url": "https://github.com/upstream/project/pull/42",
            "title": "Handle café paths", "user": { "login": "author" },
            "head": { "sha": HEAD, "ref": "feature/topic", "repo": { "full_name": "contributor/project" } },
            "base": { "sha": BASE, "repo": { "full_name": "upstream/project" } }
        })
    }

    #[test]
    fn background_deadlines_are_scoped_and_thread_local() -> Result<()> {
        assert_eq!(BACKGROUND_COMMAND_TIMEOUT.get(), None);
        let value = with_background_deadlines(|| {
            assert_eq!(
                BACKGROUND_COMMAND_TIMEOUT.get(),
                Some(Duration::from_secs(60))
            );
            assert_eq!(
                std::thread::spawn(|| BACKGROUND_COMMAND_TIMEOUT.get())
                    .join()
                    .unwrap(),
                None
            );
            let value = with_background_timeout(Duration::from_millis(100), || {
                assert_eq!(
                    BACKGROUND_COMMAND_TIMEOUT.get(),
                    Some(Duration::from_millis(100))
                );
                Ok(42)
            })?;
            assert_eq!(
                BACKGROUND_COMMAND_TIMEOUT.get(),
                Some(Duration::from_secs(60))
            );
            Ok(value)
        })?;
        assert_eq!(value, 42);
        assert_eq!(BACKGROUND_COMMAND_TIMEOUT.get(), None);
        let failed: Result<()> = with_background_deadlines(|| bail!("scope failure"));
        assert!(failed.is_err());
        assert_eq!(BACKGROUND_COMMAND_TIMEOUT.get(), None);
        let panicked = std::panic::catch_unwind(|| {
            with_background_deadlines(|| -> Result<()> { panic!("scope unwind") })
        });
        assert!(panicked.is_err());
        assert_eq!(BACKGROUND_COMMAND_TIMEOUT.get(), None);
        Ok(())
    }

    #[test]
    fn background_deadlines_skip_quick_local_commands_and_foreground() -> Result<()> {
        let client = client();
        let gh = client.api_command("GET", "user")?;
        let fetch = client.pr_fetch_command_for(&client.repo);
        assert_eq!(background_timeout_for(&gh), None);
        assert_eq!(background_timeout_for(&fetch), None);
        with_background_deadlines(|| {
            assert_eq!(background_timeout_for(&gh), Some(Duration::from_secs(60)));
            assert_eq!(
                background_timeout_for(&fetch),
                Some(Duration::from_secs(60))
            );
            assert_eq!(
                background_timeout_for(client.git_command().arg("for-each-ref")),
                None
            );
            assert_eq!(
                background_timeout_for(jj_command(&client.root).arg("root")),
                None
            );
            Ok(())
        })?;
        assert_eq!(background_timeout_for(&gh), None);
        assert_eq!(background_timeout_for(&fetch), None);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn background_runner_captures_output_without_pipe_deadlocks() -> Result<()> {
        let output = run_background(
            Command::new("sh").args([
                "-c",
                "printf 'captured output'; printf 'private stderr' >&2",
            ]),
            "read test output",
            Duration::from_secs(2),
        )?;
        assert_eq!(output, b"captured output");
        let output = run_background(
            Command::new("sh").args(["-c",
                "dd if=/dev/zero bs=65536 count=4 2>/dev/null; dd if=/dev/zero bs=65536 count=4 1>&2 2>/dev/null"]),
            "read large test output", Duration::from_secs(2),
        )?;
        assert_eq!(output, vec![0; 262144]);
        let error = run_background(
            Command::new("sh")
                .env("PRIVATE_TEST_VALUE", "secret environment")
                .args([
                    "-c",
                    "printf 'secret stdout'; printf '%s' \"$PRIVATE_TEST_VALUE\" >&2; exit 7",
                    "secret argument",
                ]),
            "run failing test",
            Duration::from_secs(2),
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("exit status"), "{error}");
        for secret in [
            "secret stdout",
            "secret environment",
            "secret argument",
            "PRIVATE_TEST_VALUE",
        ] {
            assert!(!error.contains(secret), "{error}");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn background_runner_times_out_and_kills_the_process_group() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let pid_file = temp.path().join("leader-pid");
        let survivor_file = temp.path().join("surviving-helper");
        let start = Instant::now();
        let error = run_background(
            Command::new("sh")
                .env("PRIVATE_TEST_VALUE", "secret timeout environment")
                .args(["-c", "printf '%s' \"$$\" > \"$1\"; printf '%s' \"$PRIVATE_TEST_VALUE\" >&2; (sleep 0.4; printf leaked > \"$2\") & wait", "secret timeout argument"])
                .arg(&pid_file).arg(&survivor_file),
            "run sleeping test", Duration::from_millis(100),
        ).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        let error = format!("{error:#}");
        assert!(error.contains("timed out after 100ms"), "{error}");
        assert!(!error.contains("secret"), "{error}");
        assert!(!error.contains("PRIVATE_TEST_VALUE"), "{error}");
        let pid = fs::read_to_string(pid_file)?.parse::<u32>()?;
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", "--", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success(),
            "the direct child must be reaped"
        );
        // A kill of only the leader would let the helper finish and write this file.
        // Avoid probing orphan zombies: some test environments reap them slowly.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !survivor_file.exists(),
            "the descendant survived the timeout"
        );
        Ok(())
    }

    #[test]
    fn prefetch_key_uses_transport_identity_not_cosmetic_fields() -> Result<()> {
        let client = client();
        let pr = map_pr(&pull_json(), &target(), 42, "reviewer")?;
        let key = client.prefetch_namespace(&pr, "main", None)?;
        let mut cosmetic = pr.clone();
        cosmetic.title = "New title".into();
        cosmetic.reviewer = "another-reviewer".into();
        assert_eq!(client.prefetch_namespace(&cosmetic, "main", None)?, key);
        for field in ["number", "head", "base", "source-repo", "source-ref"] {
            let mut changed = pr.clone();
            match field {
                "number" => changed.number += 1,
                "head" => changed.head_sha = BASE.into(),
                "base" => changed.base_sha = HEAD.into(),
                "source-repo" => changed.head_repo = "another/project".into(),
                "source-ref" => changed.head_ref = "another/branch".into(),
                _ => unreachable!(),
            }
            assert_ne!(
                client.prefetch_namespace(&changed, "main", None)?,
                key,
                "{field}"
            );
        }
        assert_ne!(client.prefetch_namespace(&pr, "release", None)?, key);
        for (host, repo) in [
            ("github.example", "upstream/project"),
            ("github.com", "other/project"),
        ] {
            let mut changed = client.clone();
            changed.host = host.into();
            changed.repo = repo.into();
            assert_ne!(changed.prefetch_namespace(&pr, "main", None)?, key);
        }
        let mut source = SourceRemote {
            name: "origin".into(),
            url: "https://github.com/contributor/project.git".into(),
        };
        let with_source = client.prefetch_namespace(&pr, "main", Some(&source))?;
        assert_ne!(with_source, key);
        source.name = "renamed".into();
        assert_ne!(
            client.prefetch_namespace(&pr, "main", Some(&source))?,
            with_source
        );
        source.name = "origin".into();
        source.url = "https://github.example/contributor/project.git".into();
        assert_ne!(
            client.prefetch_namespace(&pr, "main", Some(&source))?,
            with_source
        );
        Ok(())
    }

    fn listed_json(number: u64, sha: &str, owner: &str) -> Value {
        json!({
            "number": number, "headRefOid": sha,
            "url": format!("https://github.com/upstream/project/pull/{number}"),
            "headRepository": { "name": "project", "nameWithOwner": format!("{owner}/project") },
            "headRepositoryOwner": { "login": owner }
        })
    }

    #[test]
    fn parses_common_remote_urls() {
        for url in [
            "https://GitHub.com/upstream/project",
            "https://github.com/upstream/project.git",
            "https://github.com/upstream/project.git/",
            "git@github.com:upstream/project.git",
            "ssh://git@github.com/upstream/project.git",
        ] {
            assert_eq!(parse_remote_url(url).unwrap(), target(), "{url}");
        }
        assert_eq!(
            parse_remote_url("git@git.example.org:team/.github.git").unwrap(),
            Target {
                host: "git.example.org".into(),
                repo: "team/.github".into()
            }
        );
    }

    #[test]
    fn source_remote_identity_uses_the_head_repository_and_exact_host() {
        let pr = map_pr(&pull_json(), &target(), 42, "reviewer").unwrap();
        for url in [
            "https://github.com/contributor/project.git",
            "git@GitHub.com:Contributor/Project.git",
            "ssh://git@github.com/contributor/project.git",
        ] {
            assert!(source_url_matches(url, &pr), "{url}");
        }
        for url in [
            "https://github.com/upstream/project.git",
            "https://github.example/contributor/project.git",
            "https://github.com/contributor/project-extra.git",
            "https://github.com/another/project.git",
            "https://github.com/contributor/project.git/extra",
            "https://token@github.com/contributor/project.git",
            "file:///contributor/project.git",
            "git@github-alias:contributor/project.git",
        ] {
            assert!(!source_url_matches(url, &pr), "{url}");
        }
    }

    #[test]
    fn rejects_unsafe_remote_urls() {
        for url in [
            "http://github.com/o/r",
            "file:///o/r",
            "/tmp/o/r",
            "https://token@github.com/o/r",
            "https://github.com:443/o/r",
            "https://github.com/o/../r",
            "https://github.com/o/r?x=y",
            "https://github.com/o/r#fragment",
            "https://github.com/o/%2e%2e",
            "https://github.com/o/r/extra",
            "git@github.com:../r.git",
            "ssh://other@github.com/o/r",
            "https://-host/o/r",
        ] {
            assert!(parse_remote_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn parses_and_validates_explicit_identifiers() {
        assert_eq!(parse_pr_spec("42").unwrap().number, 42);
        assert_eq!(
            parse_pr_spec("https://GHE.example/team/repo/pull/42/")
                .unwrap()
                .target
                .unwrap()
                .host,
            "ghe.example"
        );
        for bad in [
            "0",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1?x",
            "18446744073709551616",
            "http://github.com/o/r/pull/1",
            "https://github.com/o/r/pull/1/files",
            "https://github.com/o/r/pull/1?x",
            "https://x@y/o/r/pull/1",
        ] {
            assert!(parse_pr_spec(bad).is_err(), "{bad}");
        }
        for good in [
            "owner/repo",
            "github.com/owner/repo",
            "ghe.example/team/.github",
            "owner/repo.name-1",
        ] {
            assert!(parse_repo_spec(good).is_ok(), "{good}");
        }
        for bad in [
            "owner",
            "owner/",
            "/repo",
            "owner/..",
            "../repo",
            "owner/%2f",
            "owner/repo?x",
            "https://github.com/o/r",
            "user@host/o/r",
            "host:443/o/r",
            "host//r",
            "a/b/c/d",
            "o/-r",
            "o/r\n",
        ] {
            assert!(parse_repo_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn explicit_url_and_repo_must_agree() {
        let pr = parse_pr_spec("https://ghe.example/upstream/project/pull/42").unwrap();
        for repo in [
            "upstream/project",
            "ghe.example/upstream/project",
            "GHE.example/Upstream/Project",
        ] {
            let repo = parse_repo_spec(repo).unwrap();
            assert_eq!(
                explicit_target(Some(&pr), Some(&repo))
                    .unwrap()
                    .unwrap()
                    .host,
                "ghe.example"
            );
        }
        for repo in ["github.com/upstream/project", "contributor/project"] {
            assert!(explicit_target(Some(&pr), Some(&parse_repo_spec(repo).unwrap())).is_err());
        }
        let number = parse_pr_spec("42").unwrap();
        assert!(explicit_target(Some(&number), None).unwrap().is_none());
        assert_eq!(
            explicit_target(
                Some(&number),
                Some(&parse_repo_spec("upstream/project").unwrap())
            )
            .unwrap(),
            Some(target())
        );
    }

    #[test]
    fn explicit_selectors_must_match_the_registered_pr_without_io() -> Result<()> {
        // This client has nonexistent workspace paths, so successful checks cannot
        // depend on GitHub discovery, Git, jj, or local workspace state.
        let mut client = client();
        let mut pr = map_pr(&pull_json(), &target(), 42, "reviewer")?;
        for (requested_pr, requested_repo) in [
            (None, None),
            (Some("42"), None),
            (Some("042"), Some("upstream/project")),
            (None, Some("GITHUB.COM/Upstream/Project")),
            (Some("https://GITHUB.COM/Upstream/Project/pull/42/"), None),
            (
                Some("https://github.com/upstream/project/pull/42"),
                Some("upstream/project"),
            ),
        ] {
            client.check_explicit_pr(&pr, requested_pr, requested_repo)?;
        }
        for (requested_pr, requested_repo) in [
            (Some("43"), None),
            (Some("https://github.com/upstream/project/pull/43"), None),
            (Some("https://ghe.example/upstream/project/pull/42"), None),
            (Some("https://github.com/other/project/pull/42"), None),
            (None, Some("ghe.example/upstream/project")),
            (None, Some("other/project")),
            (Some("42"), Some("other/project")),
            (
                Some("https://github.com/upstream/project/pull/42"),
                Some("other/project"),
            ),
            (Some("0"), None),
            (None, Some("invalid")),
        ] {
            assert!(
                client
                    .check_explicit_pr(&pr, requested_pr, requested_repo)
                    .is_err(),
                "{requested_pr:?}, {requested_repo:?}"
            );
        }
        let mut wrong_client = client.clone();
        wrong_client.repo = "other/project".into();
        assert!(wrong_client.check_explicit_pr(&pr, None, None).is_err());
        wrong_client = client.clone();
        wrong_client.reviewer = "someone-else".into();
        assert!(wrong_client.check_explicit_pr(&pr, None, None).is_err());

        // Preserve explicit_target's existing host default: a bare --repo means
        // github.com unless a PR URL supplies the host.
        client.host = "ghe.example".into();
        pr.host = "GHE.EXAMPLE".into();
        client.check_explicit_pr(&pr, Some("42"), None)?;
        client.check_explicit_pr(&pr, None, Some("GHE.example/Upstream/Project"))?;
        client.check_explicit_pr(
            &pr,
            Some("https://ghe.example/upstream/project/pull/42"),
            Some("upstream/project"),
        )?;
        assert!(
            client
                .check_explicit_pr(&pr, None, Some("upstream/project"))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn repo_view_identity_is_checked() {
        assert_eq!(map_repo_view(&json!({ "nameWithOwner": "upstream/project", "url": "https://github.com/upstream/project" })).unwrap(), target());
        assert!(map_repo_view(&json!({ "nameWithOwner": "other/project", "url": "https://github.com/upstream/project" })).is_err());
    }

    #[test]
    fn recorded_revision_accepts_only_the_exact_head_or_an_empty_direct_child() {
        let revision = RecordedRevision::parse(&format!("{HEAD}\tfalse\ttrue\t{BASE}")).unwrap();
        assert!(revision.matches(HEAD));
        assert!(revision.matches(&HEAD.to_ascii_uppercase()));
        assert!(revision.matches(BASE));
        assert!(revision.matches(&BASE.to_ascii_uppercase()));
        assert!(!revision.matches(&"f".repeat(40)));

        for (conflict, empty, parents) in [
            (false, false, BASE.to_owned()),
            (true, true, BASE.to_owned()),
            (true, false, BASE.to_owned()),
            (false, true, String::new()),
            (false, true, format!("{BASE} {}", "f".repeat(40))),
            (false, true, "f".repeat(40)),
        ] {
            let output = format!("{HEAD}\t{conflict}\t{empty}\t{parents}");
            let revision = RecordedRevision::parse(&output).unwrap();
            assert!(
                revision.matches(HEAD),
                "Exact matches remain valid: {output}"
            );
            assert!(!revision.matches(BASE), "{output}");
        }
    }

    #[test]
    fn recorded_revision_output_is_strict_and_normalizes_full_ids() {
        let revision = RecordedRevision::parse(&format!(
            "{}\tfalse\ttrue\t{}\n",
            HEAD.to_ascii_uppercase(),
            BASE.to_ascii_uppercase()
        ))
        .unwrap();
        assert_eq!(revision.commit_id, HEAD);
        assert_eq!(revision.empty_parent.as_deref(), Some(BASE));
        let sha256 = "a".repeat(64);
        let revision = RecordedRevision::parse(&format!("{sha256}\tfalse\ttrue\t")).unwrap();
        assert!(revision.matches(&sha256));
        assert!(revision.empty_parent.is_none());

        for output in [
            String::new(),
            format!("{HEAD}\tfalse\ttrue"),
            format!("{HEAD}\tfalse\ttrue\t{BASE}\textra"),
            format!("{HEAD}\tunknown\ttrue\t{BASE}"),
            format!("{HEAD}\tfalse\tunknown\t{BASE}"),
            format!("short\tfalse\ttrue\t{BASE}"),
            format!("{HEAD}\tfalse\ttrue\tshort"),
            format!("{HEAD}\tfalse\ttrue\t{BASE} "),
            format!("{HEAD}\tfalse\ttrue\t{BASE}  {BASE}"),
            format!("{HEAD}\tfalse\ttrue\t{BASE}\n{HEAD}\tfalse\ttrue\t{BASE}"),
        ] {
            assert!(RecordedRevision::parse(&output).is_err(), "{output:?}");
        }
    }

    #[test]
    fn discovery_bookmarks_prefer_the_exact_commit_and_only_fall_back_on_absence() {
        let revision = RecordedRevision::parse(&format!("{HEAD}\tfalse\ttrue\t{BASE}")).unwrap();
        let mut calls = Vec::new();
        let selected = select_discovery_bookmark(&revision, |sha| {
            calls.push(sha.to_owned());
            Ok("exact\tfalse\n".to_owned())
        })
        .unwrap();
        assert_eq!(selected, (HEAD.to_owned(), "exact".to_owned()));
        assert_eq!(calls, [HEAD]);

        calls.clear();
        let selected = select_discovery_bookmark(&revision, |sha| {
            calls.push(sha.to_owned());
            Ok(if sha == HEAD { "" } else { "parent\tfalse\n" }.to_owned())
        })
        .unwrap();
        assert_eq!(selected, (BASE.to_owned(), "parent".to_owned()));
        assert_eq!(calls, [HEAD, BASE]);

        for output in [
            None,
            Some("bad"),
            Some("a\ttrue\n"),
            Some("a\tfalse\nb\tfalse\n"),
        ] {
            calls.clear();
            assert!(
                select_discovery_bookmark(&revision, |sha| {
                    calls.push(sha.to_owned());
                    output
                        .map(str::to_owned)
                        .context("Failed bookmark inspection")
                })
                .is_err()
            );
            assert_eq!(calls, [HEAD]);
        }
    }

    #[test]
    fn discovery_bookmarks_never_search_beyond_an_eligible_single_parent() {
        for (conflict, empty, parents, expected) in [
            (false, true, BASE.to_owned(), vec![HEAD, BASE]),
            (false, false, BASE.to_owned(), vec![HEAD]),
            (true, true, BASE.to_owned(), vec![HEAD]),
            (
                false,
                true,
                format!("{BASE} {}", "f".repeat(40)),
                vec![HEAD],
            ),
            (false, true, String::new(), vec![HEAD]),
        ] {
            let revision =
                RecordedRevision::parse(&format!("{HEAD}\t{conflict}\t{empty}\t{parents}"))
                    .unwrap();
            let mut calls = Vec::new();
            assert!(
                select_discovery_bookmark(&revision, |sha| {
                    calls.push(sha.to_owned());
                    Ok(String::new())
                })
                .is_err()
            );
            assert_eq!(calls, expected);
        }
    }

    #[test]
    fn only_one_unconflicted_local_bookmark_is_accepted() {
        assert_eq!(
            select_bookmark("feature/topic\tfalse\n")
                .unwrap()
                .as_deref(),
            Some("feature/topic")
        );
        assert_eq!(select_bookmark("").unwrap(), None);
        for bad in [
            "\n",
            "a\ttrue\n",
            "a\tfalse\nb\tfalse\n",
            "a\tfalse\nb\ttrue\n",
            "a*\tfalse\n",
            "a\tfalse\na\tfalse\n",
            "a\tunknown\n",
            "a\n",
        ] {
            assert!(select_bookmark(bad).is_err(), "{bad:?}");
        }
        for bad in [
            "-option",
            "a..b",
            "a.lock",
            ".hidden",
            "a/.hidden",
            "a@{b",
            "a b",
            "a:b",
            "a\\b",
        ] {
            assert!(validate_bookmark(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn auto_lookup_filters_exact_sha_and_rejects_ambiguity() {
        let candidates = serde_json::from_value(json!([
            listed_json(1, BASE, "other"),
            listed_json(42, HEAD, "contributor")
        ]))
        .unwrap();
        let selected = select_pr(candidates, HEAD, &target()).unwrap();
        assert_eq!(selected.number, 42);
        assert_eq!(selected.head_repo, "contributor/project");
        let candidates = serde_json::from_value(json!([
            listed_json(42, HEAD, "one"),
            listed_json(43, HEAD, "two")
        ]))
        .unwrap();
        assert!(select_pr(candidates, HEAD, &target()).is_err());
        let candidates = serde_json::from_value(json!([listed_json(1, BASE, "other")])).unwrap();
        assert!(select_pr(candidates, HEAD, &target()).is_err());
        let candidates = serde_json::from_value(Value::Array(
            (1..=100).map(|n| listed_json(n, BASE, "other")).collect(),
        ))
        .unwrap();
        assert!(select_pr(candidates, HEAD, &target()).is_err());
    }

    #[test]
    fn maps_rest_pr_including_fork_and_authenticated_reviewer() {
        let pr = map_pr(&pull_json(), &target(), 42, "reviewer").unwrap();
        assert_eq!(
            pr,
            PullRequest {
                host: "github.com".into(),
                repo: "upstream/project".into(),
                number: 42,
                url: "https://github.com/upstream/project/pull/42".into(),
                title: "Handle café paths".into(),
                head_sha: HEAD.into(),
                base_sha: BASE.into(),
                head_repo: "contributor/project".into(),
                head_ref: "feature/topic".into(),
                author: "author".into(),
                reviewer: "reviewer".into(),
            }
        );
    }

    #[test]
    fn refuses_closed_merged_or_inconsistent_pr_responses() {
        for (pointer, replacement) in [
            ("/state", json!("closed")),
            ("/merged", json!(true)),
            ("/merged", Value::Null),
            ("/merged_at", json!("2026-01-01T00:00:00Z")),
            ("/number", json!(43)),
            (
                "/html_url",
                json!("https://other.example/upstream/project/pull/42"),
            ),
            ("/head/sha", json!("HEAD")),
            ("/base/sha", json!("--help")),
            ("/head/repo", Value::Null),
            ("/base/repo/full_name", json!("other/project")),
        ] {
            let mut value = pull_json();
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                map_pr(&value, &target(), 42, "reviewer").is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn sha_and_blob_paths_cannot_inject_revisions_or_traverse() {
        for sha in [HEAD.to_owned(), HEAD.to_uppercase(), "a".repeat(64)] {
            assert!(validate_sha(&sha).is_ok());
        }
        for sha in [
            "HEAD",
            "main",
            "abc123",
            "--help",
            "HEAD:path",
            "HEAD^{commit}",
            &"z".repeat(40),
        ] {
            assert!(validate_sha(sha).is_err(), "{sha}");
        }
        for path in [
            "src/lib.rs",
            "a b/café.txt",
            "a:colon",
            "--option",
            "line\nbreak",
        ] {
            assert!(validate_path(path).is_ok(), "{path}");
        }
        for path in [
            "", "/abs", "../a", "a/../b", "./a", "a//b", "a/./b", "a/", "a\\b", "a\0b",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn maps_paginated_files_and_enforces_api_cap() {
        let files: Vec<ChangedFile> = decode_pages(br#"[[{"filename":"new.rs","status":"renamed","previous_filename":"old.rs"}],[{"filename":"binary","status":"added","patch":null}]]"#).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].previous_filename.as_deref(), Some("old.rs"));
        assert!(files[0].patch.is_none());
        assert!(files[1].patch.is_none());
        assert!(decode_pages::<Value>(br#"[{"id":1}]"#).is_err());
        assert!(decode_pages::<Value>(b"[[], []]").unwrap().is_empty());
        assert!(check_file_count(2999).is_ok());
        assert!(check_file_count(3000).is_err());
        assert!(check_file_count(3001).is_err());
    }

    #[test]
    fn pending_review_is_scoped_to_authenticated_reviewer_across_pages() {
        let reviews: Vec<Value> = decode_pages(br#"[[{"state":"PENDING","user":{"login":"other"}},{"state":"APPROVED","user":{"login":"reviewer"}}],[{"state":"PENDING","user":{"login":"Reviewer"}},{"state":"COMMENTED","user":null}]]"#).unwrap();
        assert!(has_pending_review(&reviews, "reviewer").unwrap());
        assert!(!has_pending_review(&reviews[..2], "reviewer").unwrap());
        assert!(has_pending_review(&[json!({"user": {"login": "reviewer"}})], "reviewer").is_err());
    }

    #[test]
    fn commands_use_explicit_get_host_headers_and_pagination_without_a_shell() {
        let client = client();
        let mut command = client
            .api_command("GET", "repos/upstream/project/pulls/42/files?per_page=100")
            .unwrap();
        command.args(["--paginate", "--slurp"]);
        assert_eq!(command.get_program(), "gh");
        let args: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|w| w == ["--method", "GET"]));
        assert!(args.windows(2).any(|w| w == ["--hostname", "github.com"]));
        assert!(args.contains(&API_VERSION));
        for (name, value) in [
            ("GH_HOST", "github.com"),
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_PAGER", "cat"),
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(key, val)| key == name && val == Some(std::ffi::OsStr::new(value)))
            );
        }
        assert!(
            command
                .get_envs()
                .any(|(key, val)| key == "GH_REPO" && val.is_none())
        );
        let git = client.git_command();
        assert!(
            git.get_envs()
                .any(|(key, val)| key == "GIT_NO_LAZY_FETCH"
                    && val == Some(std::ffi::OsStr::new("1")))
        );
        let jj = jj_command(&client.root);
        assert!(jj.get_args().any(|arg| arg == "--ignore-working-copy"));
    }

    #[test]
    fn prefetch_transport_disables_terminal_gh_and_askpass_prompts() {
        let client = client();
        for repo in [client.repo.as_str(), "contributor/project"] {
            let command = client.pr_fetch_command_for(repo);
            for (name, value) in [
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_ASKPASS", ""),
                ("GH_PROMPT_DISABLED", "1"),
                ("GH_HOST", "github.com"),
                ("GIT_ALLOW_PROTOCOL", "https"),
            ] {
                assert!(
                    command
                        .get_envs()
                        .any(|(key, val)| key == name && val == Some(std::ffi::OsStr::new(value))),
                    "{name}"
                );
            }
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            for config in [
                "credential.interactive=false",
                "credential.helper=",
                "credential.https://github.com.helper=!gh auth git-credential",
            ] {
                assert!(
                    args.windows(2).any(|pair| pair == ["-c", config]),
                    "{config}"
                );
            }
        }
    }

    #[test]
    fn rejects_wrong_client_and_unpinned_submission_before_any_io() {
        let client = client();
        let mut pr = map_pr(&pull_json(), &target(), 42, "reviewer").unwrap();
        assert!(client.check_identity(&pr).is_ok());
        assert!(client.submit(&pr, &json!({"event": "COMMENT"})).is_err());
        assert!(client.submit(&pr, &json!({"commit_id": BASE})).is_err());
        pr.reviewer = "someone-else".into();
        assert!(client.check_identity(&pr).is_err());
        assert!(
            client
                .checkout_matches(&pr)
                .unwrap_err()
                .to_string()
                .contains("reviewer does not match")
        );
        assert!(
            client
                .ensure_checkout(&pr)
                .unwrap_err()
                .to_string()
                .contains("reviewer does not match")
        );
        pr.reviewer = "reviewer".into();
        pr.repo = "other/project".into();
        assert!(client.check_identity(&pr).is_err());
        assert!(client.pr_endpoint(0).is_err());
        assert!(client.review_comments(42, 0).is_err());
    }
}
