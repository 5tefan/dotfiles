#![cfg(unix)]

//! Black-box tests: real jj/Git objects, a strict offline gh, and a non-GUI editor.
//! All environment changes and repository operations are confined to child processes.

mod support;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use zed_pr_review::github::Client;
use zed_pr_review::session::Session;
use zed_pr_review::store::{Store, prepare_directory};

const PR_URL: &str = "https://github.com/acme/widgets/pull/17";
const PR_ENDPOINT: &str = "repos/acme/widgets/pulls/17";
const PR_NODE_ID: &str = "PR_fixture17";
const FILE: &str = "review file.txt";
const SUMMARY: &str = "Review summary\n\nLiteral $HOME, `backticks`, ; and $(expressions).";

fn git_executable() -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").context("Missing PATH")?)
        .map(|path| path.join("git"))
        .find(|path| path.is_file())
        .context("Git is required")?
        .canonicalize()
        .context("Cannot resolve Git executable")
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    repo: PathBuf,
    state: PathBuf,
    editor: PathBuf,
    path: OsString,
    head: String,
    base: String,
    review_file: Option<PathBuf>,
}

impl Fixture {
    fn new() -> Result<Self> {
        Self::with_init_args(&[])
    }

    fn review() -> Result<Self> {
        let mut fixture = Self::new()?;
        fixture.use_default_state();
        fixture.review_file = Some(fixture.prepare_session()?.join("head").join(FILE));
        Ok(fixture)
    }

    fn with_init_args(init_args: &[&str]) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("zed-pr-review-cli-")
            .tempdir()?;
        let root = temp.path().canonicalize()?;
        let repo = root.join("checkout with spaces");
        let state = root.join("private state");
        let bin = root.join("bin");
        for directory in [&repo, &bin, &root.join("home"), &root.join("tmp")] {
            fs::create_dir_all(directory)?;
        }
        fs::write(
            root.join("jj.toml"),
            "[user]\nname = 'CLI Tests'\nemail = 'cli@example.test'\n[ui]\npager = 'cat'\n",
        )?;
        let editor = bin.join("review editor");
        for (tool, destination) in [
            ("gh", bin.join("gh")),
            ("editor", editor.clone()),
            ("zed", bin.join("zed")),
        ] {
            support::install(&destination, &json!({"tool": tool}))?;
        }
        // Even refusal-path tests must never reach a real GitHub transport if
        // the CLI regresses and fetches before validating its arguments.
        support::install(
            &bin.join("git"),
            &json!({"tool": "cli-git", "real": git_executable()?}),
        )?;
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").context("PATH is unset")?,
        ));
        let mut fixture = Self {
            _temp: temp,
            root,
            repo,
            state,
            editor,
            path: std::env::join_paths(paths)?,
            head: String::new(),
            base: String::new(),
            review_file: None,
        };
        let mut args = vec!["git", "init"];
        args.extend_from_slice(init_args);
        fixture.jj(&args)?;
        // A real empty base commit is needed: jj's synthetic root is not a Git object.
        fixture.jj(&["describe", "-m", "Base commit"])?;
        fixture.jj(&["new", "-m", "PR head"])?;
        fs::write(fixture.repo.join(FILE), "one\ntwo\nthree\n")?;
        fixture.jj(&["describe", "-m", "Review fixture snapshot"])?;
        fixture.jj(&["bookmark", "create", "topic"])?;
        fixture.head = fixture.sha("@")?;
        fixture.base = fixture.sha("@-")?;
        fixture.set_control(json!({
            "head": fixture.head,
            "base": fixture.base,
            "post": "success",
            "editor_body": SUMMARY,
            "reviews": [],
            "review_comments": {},
        }))?;
        Ok(fixture)
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        // Do not inherit credentials, Git/JJ context, editor settings, or Zed selections.
        command.env_clear().current_dir(&self.repo).envs([
            ("PATH", self.path.clone()),
            ("HOME", self.root.join("home").into_os_string()),
            ("TMPDIR", self.root.join("tmp").into_os_string()),
            ("XDG_CONFIG_HOME", self.root.join("home").into_os_string()),
            ("JJ_CONFIG", self.root.join("jj.toml").into_os_string()),
            ("JJ_PAGER", "cat".into()),
            // Test-level parallelism is enough for these tiny jj repositories.
            ("RAYON_NUM_THREADS", "1".into()),
            ("PAGER", "cat".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_EDITOR", "true".into()),
            ("ZED_PR_REVIEW_PREFETCH", "0".into()),
            ("EDITOR", shell_quote(&self.editor.to_string_lossy()).into()),
            (
                "ZPR_TEST_CONTROL",
                self.root.join("control.json").into_os_string(),
            ),
            (
                "ZPR_TEST_GH_LOG",
                self.root.join("gh.jsonl").into_os_string(),
            ),
            (
                "ZPR_TEST_EDITOR_LOG",
                self.root.join("editor.jsonl").into_os_string(),
            ),
            ("ZPR_TEST_STATE", self.state.clone().into_os_string()),
            (
                "ZPR_TEST_ZED_LOG",
                self.root.join("zed.jsonl").into_os_string(),
            ),
        ]);
        command
    }

    fn cli(&self, args: &[&str]) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_zed-pr-review"));
        command
            .arg("--directory")
            .arg(&self.repo)
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .env(
                "ZED_FILE",
                self.review_file
                    .clone()
                    .unwrap_or_else(|| self.repo.join(FILE)),
            )
            .env("ZED_ROW", "1")
            .env("ZED_COLUMN", "1")
            .env("ZED_SELECTED_TEXT", "one");
        command
    }

    fn execute(&self, command: Command, input: &str) -> Result<Output> {
        let output = bounded_output(command, input, &self.root)?;
        for call in self.calls()? {
            ensure!(
                call.get("unexpected").is_none(),
                "Fake gh rejected a call: {call}"
            );
        }
        Ok(output)
    }

    fn run(&self, args: &[&str], input: &str) -> Result<Output> {
        self.execute(self.cli(args), input)
    }

    fn ready_cli(&self, args: &[&str]) -> Command {
        let mut command = self.cli(&["ready"]);
        command
            .args(args)
            .env("EDITOR", self.root.join("no-editor"));
        for key in ["ZED_FILE", "ZED_ROW", "ZED_COLUMN", "ZED_SELECTED_TEXT"] {
            command.env_remove(key);
        }
        command
    }

    fn run_ready(&self, args: &[&str]) -> Result<Output> {
        self.execute(self.ready_cli(args), "")
    }

    fn saved(&self, args: &[&str], input: &str) -> Result<Output> {
        let directory = self
            .draft_path()?
            .parent()
            .context("Missing draft directory")?
            .to_owned();
        let mut command = self.cli(&[]);
        command.arg("--draft-dir").arg(directory).args(args);
        self.execute(command, input)
    }

    fn comment(&self, row: u32, selected: &str, body: &str) -> Result<()> {
        let mut command = self.cli(&["comment"]);
        command
            .env("ZED_ROW", row.to_string())
            .env("ZED_SELECTED_TEXT", selected);
        success(&self.execute(command, body)?);
        Ok(())
    }

    fn jj(&self, args: &[&str]) -> Result<String> {
        let mut command = self.command("jj");
        command.args(["--no-pager", "--color=never"]).args(args);
        let output = bounded_output(command, "", &self.root)?;
        ensure!(
            output.status.success(),
            "jj {args:?}: {}",
            describe(&output)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn sha(&self, revision: &str) -> Result<String> {
        self.jj(&[
            "--ignore-working-copy",
            "log",
            "-r",
            revision,
            "--no-graph",
            "-T",
            "commit_id",
        ])
    }

    fn previous_bookmarks(&self) -> Result<BTreeMap<String, String>> {
        let listing = self.jj(&["--ignore-working-copy", "bookmark", "list", "-T",
            r#"if(name.starts_with("zed-pr-review/previous-"), name ++ "\t" ++ normal_target.commit_id() ++ "\n")"#])?;
        listing
            .lines()
            .map(|line| {
                let (name, sha) = line
                    .split_once('\t')
                    .context("Malformed previous bookmark")?;
                Ok((name.to_owned(), sha.to_owned()))
            })
            .collect()
    }

    fn checkout_previous(&self, session: &Path) -> Result<String> {
        let receipt = read_json(&session.parent().unwrap().join("last-checkout.json"))?;
        let previous = receipt["previous_commit"]
            .as_str()
            .context("Missing previous commit in checkout receipt")?;
        assert_eq!(
            previous.len(),
            40,
            "Receipt must retain the full commit SHA"
        );
        assert_eq!(self.sha(previous)?, previous);
        Ok(previous.to_owned())
    }

    fn set_immutable_heads(&self, heads: &[&str]) -> Result<()> {
        let mut revisions = vec!["root()".to_owned()];
        revisions.extend(heads.iter().map(|head| format!("present({head})")));
        self.jj(&[
            "config",
            "set",
            "--repo",
            "revset-aliases.\"immutable_heads()\"",
            &revisions.join(" | "),
        ])?;
        Ok(())
    }

    fn assert_empty_child(&self, head: &str) -> Result<String> {
        let child = self.sha("@")?;
        assert_ne!(child, head);
        assert_eq!(
            self.sha("@-")?,
            head,
            "Exactly one parent must be the PR head"
        );
        assert_eq!(self.sha("@ & empty() & ~conflicts()")?, child);
        Ok(child)
    }

    fn empty_immutable_head_child(&self) -> Result<String> {
        self.jj(&[
            "new",
            "-m",
            "Empty working copy above the immutable PR head",
        ])?;
        self.set_immutable_heads(&[&self.head])?;
        assert_eq!(
            self.jj(&[
                "--ignore-working-copy",
                "log",
                "-r",
                &self.head,
                "--no-graph",
                "-T",
                "immutable",
            ])?,
            "true"
        );
        self.assert_empty_child(&self.head)
    }

    fn nonempty_child(&self, description: &str) -> Result<String> {
        self.jj(&["new", "-m", description])?;
        fs::write(self.repo.join("recorded local.txt"), "Not part of the PR\n")?;
        self.jj(&["describe", "-m", description])?;
        let child = self.sha("@")?;
        assert_eq!(self.sha("@ & ~empty()")?, child);
        Ok(child)
    }

    fn control(&self) -> Result<Value> {
        read_json(&self.root.join("control.json"))
    }

    fn set_control(&self, control: Value) -> Result<()> {
        fs::write(
            self.root.join("control.json"),
            serde_json::to_vec(&control)?,
        )?;
        Ok(())
    }

    fn configure(&self, key: &str, value: Value) -> Result<()> {
        let mut control = self.control()?;
        control[key] = value;
        self.set_control(control)
    }

    fn accept_delayed_review(&self) -> Result<()> {
        let mut control = self.control()?;
        let review = control["delayed_review"].take();
        ensure!(!review.is_null(), "No delayed review to accept");
        let id = review["id"].to_string();
        control["reviews"].as_array_mut().unwrap().push(review);
        control["review_comments"][&id] = control["delayed_comments"].take();
        self.set_control(control)
    }

    fn calls(&self) -> Result<Vec<Value>> {
        read_json_lines(&self.root.join("gh.jsonl"))
    }

    fn posts(&self) -> Result<Vec<Value>> {
        Ok(self
            .calls()?
            .into_iter()
            .filter(|call| {
                call["method"] == "POST"
                    && call["endpoint"] == "repos/acme/widgets/pulls/17/reviews"
            })
            .collect())
    }

    fn ready_mutations(&self) -> Result<Vec<Value>> {
        Ok(self
            .calls()?
            .into_iter()
            .filter(|call| call.get("mutation").is_some())
            .collect())
    }

    fn assert_ready_mutations(&self, expected: &[&str]) -> Result<()> {
        let calls = self.ready_mutations()?;
        let actual: Vec<_> = calls
            .iter()
            .map(|call| call["mutation"].as_str().unwrap())
            .collect();
        assert_eq!(actual, expected, "{calls:?}");
        let control = self.control()?;
        let host = control["host"].as_str().unwrap_or("github.com");
        for call in &calls {
            assert_ready_api_post(call, host);
            let endpoint = if call["mutation"] == "ready" {
                "graphql"
            } else {
                "repos/acme/widgets/pulls/17/requested_reviewers"
            };
            assert_eq!(call["endpoint"], endpoint);
        }
        Ok(())
    }

    fn assert_stateless_ready(&self) -> Result<()> {
        assert!(
            !self.state.exists(),
            "ready must not create a Store or lock"
        );
        assert!(self.drafts()?.is_empty());
        assert!(self.posts()?.is_empty());
        assert!(self.editors()?.is_empty());
        Ok(())
    }

    fn editors(&self) -> Result<Vec<Value>> {
        read_json_lines(&self.root.join("editor.jsonl"))
    }

    fn drafts(&self) -> Result<Vec<PathBuf>> {
        if !self.state.exists() {
            return Ok(vec![]);
        }
        let mut drafts = Vec::new();
        for entry in fs::read_dir(&self.state)? {
            let path = entry?.path().join("draft.json");
            if path.is_file() {
                drafts.push(path);
            }
        }
        Ok(drafts)
    }

    fn draft_path(&self) -> Result<PathBuf> {
        let drafts = self.drafts()?;
        ensure!(
            drafts.len() == 1,
            "Expected exactly one draft, found {drafts:?}"
        );
        Ok(drafts[0].clone())
    }

    fn draft(&self) -> Result<Value> {
        read_json(&self.draft_path()?)
    }

    fn assert_no_comments(&self) -> Result<()> {
        for path in self.drafts()? {
            assert_eq!(read_json(&path)?["comments"], json!([]));
        }
        assert!(self.posts()?.is_empty());
        Ok(())
    }

    fn fail_submission(&self, mode: &str) -> Result<Value> {
        self.comment(1, "one\ntwo\n", "Keep this comment")?;
        self.configure("post", json!(mode))?;
        failure(&self.run(&["approve"], "")?);
        let draft = self.draft()?;
        assert!(!draft["attempt"].is_null());
        assert!(draft["submitted"].is_null());
        assert_eq!(self.posts()?.len(), 1);
        Ok(draft)
    }
}

impl Fixture {
    fn use_default_state(&mut self) {
        self.state = self.repo.join("scratch/zed-pr-review");
    }

    // Deliberately do not pass --state-dir: exercise main's root-based default.
    fn default_cli(&self, args: &[&str]) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_zed-pr-review"));
        command
            .arg("--directory")
            .arg(&self.repo)
            .args(args)
            .env("ZED_FILE", self.repo.join(FILE))
            .env("ZED_ROW", "1")
            .env("ZED_COLUMN", "1")
            .env("ZED_SELECTED_TEXT", "one");
        command
    }

    fn run_default(&self, args: &[&str], input: &str) -> Result<Output> {
        self.execute(self.default_cli(args), input)
    }

    fn run_session(&self, session: &Path, args: &[&str], input: &str) -> Result<Output> {
        let mut command = self.default_cli(args);
        command.env("ZED_FILE", session.join("head").join(FILE));
        self.execute(command, input)
    }

    fn zed_calls(&self) -> Result<Vec<Value>> {
        read_json_lines(&self.root.join("zed.jsonl"))
    }

    fn active_directory(&self) -> Result<PathBuf> {
        let active = read_json(&self.state.join("active.json"))?;
        Ok(PathBuf::from(
            active["directory"]
                .as_str()
                .context("Missing active directory")?,
        ))
    }

    fn prepare_session(&self) -> Result<PathBuf> {
        let mut command = self.command(std::env::current_exe()?);
        command
            .args(["--exact", "session_fixture_child", "--nocapture"])
            .env("ZPR_PREPARE_SESSION", "1");
        success(&self.execute(command, "")?);
        self.active_directory()
    }

    fn git_in(&self, git_dir: &Path, args: &[&str], input: &str) -> Result<String> {
        let mut command = self.command("git");
        command
            .arg("--no-pager")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "CLI Transport")
            .env("GIT_AUTHOR_EMAIL", "cli@example.test")
            .env("GIT_COMMITTER_NAME", "CLI Transport")
            .env("GIT_COMMITTER_EMAIL", "cli@example.test");
        let output = bounded_output(command, input, &self.root)?;
        ensure!(
            output.status.success(),
            "git {args:?}: {}",
            describe(&output)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let git_dir = PathBuf::from(self.jj(&["--ignore-working-copy", "git", "root"])?);
        let mut scoped = vec![
            "--work-tree",
            self.repo.to_str().context("Non-UTF-8 fixture root")?,
        ];
        scoped.extend_from_slice(args);
        self.git_in(&git_dir, &scoped, "")
    }

    fn assert_default_state_ignored(&self) -> Result<()> {
        self.jj(&["status"])?;
        assert_eq!(
            self.sha("@")?,
            self.head,
            "Normal jj snapshot must ignore review state"
        );
        let files = self.jj(&["file", "list"])?;
        assert!(
            !files
                .lines()
                .any(|path| path.starts_with("scratch/zed-pr-review")),
            "{files}"
        );
        assert_eq!(
            self.git(&[
                "check-ignore",
                "--no-index",
                "scratch/zed-pr-review/.gitignore"
            ])?,
            "scratch/zed-pr-review/.gitignore"
        );
        Ok(())
    }
}

// Preparing sessions through the public API avoids repeating transport in mapping
// tests. A child keeps PATH, HOME and JJ_CONFIG isolated from parallel Rust tests.
#[test]
fn session_fixture_child() -> Result<()> {
    if std::env::var_os("ZPR_PREPARE_SESSION").is_none() {
        return Ok(());
    }
    let root = std::env::current_dir()?;
    let pr_url = std::env::var("ZPR_PREPARE_SESSION_PR").unwrap_or_else(|_| PR_URL.into());
    let (client, pr) = Client::resolve(&root, &pr_url, None)?;
    let state = prepare_directory(
        &client.root,
        &client.git_dir,
        &client.root.join("scratch/zed-pr-review"),
        Some(&pr.head_sha),
    )?;
    let store = Store::open(&state, &pr)?;
    let session = Session::prepare(&client, &store, &pr)?;
    session.activate(&state)?;
    Ok(())
}

struct OpenFixture {
    fixture: Fixture,
    common: String,
    first: String,
}

impl OpenFixture {
    fn new(colocate: bool) -> Result<Self> {
        let mut fixture = Fixture::with_init_args(&[if colocate {
            "--colocate"
        } else {
            "--no-colocate"
        }])?;
        fixture.use_default_state();
        fixture.jj(&["bookmark", "delete", "topic"])?;
        // No branch name can identify the PR, and @ is unrelated to its history.
        fixture.jj(&["bookmark", "create", "unrelated-local-work"])?;
        fs::write(fixture.repo.join("keep local.txt"), "Unsnapshotted work\n")?;
        let server = fixture.root.join("remote.git");
        fixture.git_in(&server, &["init", "--bare"], "")?;
        let common = Self::commit(
            &fixture,
            &server,
            &[
                (FILE, "one\ntwo\nthree\n"),
                ("removed.txt", "old\n"),
                ("unchanged.txt", "same\n"),
            ],
            None,
        )?;
        let first = Self::commit(
            &fixture,
            &server,
            &[
                (FILE, "ONE\ntwo\nthree\n"),
                ("removed.txt", "old\n"),
                ("unchanged.txt", "same\n"),
                ("early change.txt", "first commit\n"),
            ],
            Some(&common),
        )?;
        fixture.head = Self::commit(
            &fixture,
            &server,
            &[
                (FILE, "ONE\ntwo\nTHREE\n"),
                ("unchanged.txt", "same\n"),
                ("early change.txt", "first commit\n"),
                ("late change.txt", "second commit\n"),
            ],
            Some(&first),
        )?;
        fixture.base = Self::commit(
            &fixture,
            &server,
            &[
                (FILE, "one\nbase only\nthree\n"),
                ("removed.txt", "old\n"),
                ("unchanged.txt", "same\n"),
                ("base-only.txt", "not part of this PR\n"),
            ],
            Some(&common),
        )?;
        for (reference, sha) in [
            ("refs/pull/17/head", &fixture.head),
            ("refs/heads/main", &fixture.base),
            // Fetching the apparent head branch instead of the exact PR ref is wrong.
            ("refs/heads/topic", &fixture.base),
        ] {
            fixture.git_in(&server, &["update-ref", reference, sha], "")?;
        }
        fixture.configure("head", json!(fixture.head))?;
        fixture.configure("base", json!(fixture.base))?;
        fixture.configure("head_repo", json!("contributor/widgets"))?;
        fixture.configure("files", json!([
            {"filename": FILE, "status": "modified", "patch": "@@ -1,3 +1,3 @@\n-one\n+ONE\n two\n-three\n+THREE"},
            {"filename": "early change.txt", "status": "added", "patch": "@@ -0,0 +1 @@\n+first commit"},
            {"filename": "late change.txt", "status": "added", "patch": "@@ -0,0 +1 @@\n+second commit"},
            {"filename": "removed.txt", "status": "removed", "patch": "@@ -1 +0,0 @@\n-old"}
        ]))?;
        install_local_pr_transport(&fixture, &[(17, "main")])?;
        Ok(Self {
            fixture,
            common,
            first,
        })
    }

    fn commit(
        fixture: &Fixture,
        server: &Path,
        files: &[(&str, &str)],
        parent: Option<&str>,
    ) -> Result<String> {
        let mut tree = String::new();
        for (path, contents) in files {
            let blob = fixture.git_in(server, &["hash-object", "-w", "--stdin"], contents)?;
            tree.push_str(&format!("100644 blob {blob}\t{path}\n"));
        }
        let tree = fixture.git_in(server, &["mktree"], &tree)?;
        let mut args = vec!["commit-tree", &tree];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        fixture.git_in(server, &args, "Offline PR commit\n")
    }
}

// Only explicit PR refs and their corresponding base refs may reach the local
// bare server. No configured remote, branch substitution, or live transport is used.
fn install_local_pr_transport(fixture: &Fixture, prs: &[(u64, &str)]) -> Result<()> {
    let bases: BTreeMap<_, _> = prs
        .iter()
        .map(|(number, base)| (number.to_string(), *base))
        .collect();
    support::install(
        &fixture.root.join("bin/git"),
        &json!({
            "tool": "cli-git", "mode": "local-pr", "real": git_executable()?,
            "root": fixture.root, "bases": bases,
        }),
    )
}

const PULLS_ENDPOINT: &str = "repos/acme/widgets/pulls?state=open&per_page=100";

fn navigation_fixture() -> Result<Fixture> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    // Both sides must contain a real registered file, not a missing BASE path.
    fixture.jj(&["new", "-m", "Navigation PR head"])?;
    fs::write(fixture.repo.join(FILE), "one\ntwo\nTHREE\n")?;
    fixture.jj(&["describe", "-m", "Navigation PR head"])?;
    fixture.base = fixture.head.clone();
    fixture.head = fixture.sha("@")?;
    fixture.configure("head", json!(fixture.head))?;
    fixture.configure("base", json!(fixture.base))?;
    fixture.configure(
        "files",
        json!([{
            "filename": FILE, "status": "modified",
            "patch": "@@ -1,3 +1,3 @@\n one\n two\n-three\n+THREE"
        }]),
    )?;
    fixture.configure("pulls_pages", json!([[17]]))?;
    // These aliases exist only in the temporary JJ_CONFIG. They deliberately
    // perform real local navigation so a mistaken fallback changes the checkout.
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(fixture.root.join("jj.toml"))?;
    writeln!(
        config,
        r#"
[aliases]
l = ["log", "-r", "@", "--no-graph", "-T", "description"]
sb = ["edit", "@-"]
sf = ["edit", "@+"]
"#
    )?;
    let real_jj = std::env::split_paths(&std::env::var_os("PATH").context("Missing PATH")?)
        .map(|path| path.join("jj"))
        .find(|path| path.is_file())
        .context("jj is required")?
        .canonicalize()?;
    for (name, tool, real) in [
        ("jj", "cli-jj", real_jj.clone()),
        ("git", "cli-git", git_executable()?),
        ("viddy", "cli-jj", real_jj),
    ] {
        support::install(
            &fixture.root.join("bin").join(name),
            &json!({
                "tool": tool, "mode": "navigation", "program": name,
                "root": fixture.root, "real": real,
            }),
        )?;
    }
    Ok(fixture)
}

fn navigation_session() -> Result<(Fixture, PathBuf)> {
    let fixture = navigation_fixture()?;
    let session = fixture.prepare_session()?;
    success(&fixture.run_session(&session, &["comment"], "Keep this private navigation draft")?);
    fixture.jj(&["bookmark", "delete", "topic"])?;
    Ok((fixture, session))
}

fn navigation_command(fixture: &Fixture, args: &[&str], file: Option<&Path>) -> Command {
    let mut command = fixture.default_cli(args);
    if let Some(file) = file {
        command.env("ZED_FILE", file);
    } else {
        command.env_remove("ZED_FILE");
    }
    // Stack shortcuts must not require comment selections.
    for key in ["ZED_ROW", "ZED_COLUMN", "ZED_SELECTED_TEXT"] {
        command.env_remove(key);
    }
    command
}

fn navigation_calls(fixture: &Fixture) -> Result<Vec<Value>> {
    read_json_lines(&fixture.root.join("navigation.jsonl"))
}

fn navigation_clear_calls(fixture: &Fixture) -> Result<()> {
    for name in ["navigation.jsonl", "gh.jsonl"] {
        fs::write(fixture.root.join(name), "")?;
    }
    Ok(())
}

fn navigation_assert_local_alias(fixture: &Fixture, alias: &str) -> Result<()> {
    let calls = navigation_calls(fixture)?;
    assert!(
        calls.iter().all(|call| call["program"] != "viddy"),
        "Navigation must never invoke viddy: {calls:?}"
    );
    let aliases: Vec<_> = calls
        .iter()
        .filter(|call| {
            call["program"] == "jj"
                && call["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "l" || arg == "sb" || arg == "sf")
        })
        .collect();
    assert_eq!(aliases.len(), 1, "Expected one local alias call: {calls:?}");
    assert!(
        aliases[0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == alias)
    );
    assert_eq!(aliases[0]["cwd"], json!(fixture.repo));
    assert!(
        fixture.calls()?.is_empty(),
        "Local navigation must not invoke gh"
    );
    Ok(())
}

struct NavigationCheckpoint {
    head: String,
    active: Vec<u8>,
    drafts: Vec<(PathBuf, Vec<u8>)>,
}

impl NavigationCheckpoint {
    fn capture(fixture: &Fixture) -> Result<Self> {
        let mut drafts = fixture.drafts()?;
        drafts.sort();
        Ok(Self {
            head: fixture.sha("@")?,
            active: fs::read(fixture.state.join("active.json"))?,
            drafts: drafts
                .into_iter()
                .map(|path| {
                    let bytes = fs::read(&path)?;
                    Ok((path, bytes))
                })
                .collect::<Result<_>>()?,
        })
    }

    fn assert_preserved(&self, fixture: &Fixture) -> Result<()> {
        let after = Self::capture(fixture)?;
        assert_eq!(
            after.head, self.head,
            "Navigation changed the recorded checkout"
        );
        assert_eq!(
            after.active, self.active,
            "Navigation replaced the active session"
        );
        assert_eq!(
            after.drafts, self.drafts,
            "Navigation changed saved draft bytes"
        );
        assert!(fixture.calls()?.iter().all(|call| call["method"] != "POST"));
        assert!(fixture.editors()?.is_empty());
        assert!(fixture.zed_calls()?.is_empty());
        Ok(())
    }

    fn assert_unchanged(&self, fixture: &Fixture) -> Result<()> {
        self.assert_preserved(fixture)?;
        for call in navigation_calls(fixture)? {
            assert_ne!(
                call["program"], "viddy",
                "Review mode must not run a watcher"
            );
            let args = call["args"].as_array().unwrap();
            for forbidden in [
                "l", "sb", "sf", "edit", "new", "describe", "abandon", "restore", "fetch", "push",
            ] {
                assert!(
                    !args.iter().any(|arg| arg == forbidden),
                    "Unexpected mutation/fallback: {call}"
                );
            }
        }
        Ok(())
    }
}

fn navigation_failure(output: &Output) {
    failure(output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "Must reject navigation, not fail to parse the subcommand: {}",
        describe(output)
    );
}

fn navigation_children(fixture: &Fixture) -> Result<()> {
    fixture.configure("prs", json!({
        "18": {"head": "1818181818181818181818181818181818181818", "base": fixture.head,
               "head_ref": "topic-child-one", "base_ref": "topic", "title": "First child", "draft": false},
        "19": {"head": "1919191919191919191919191919191919191919", "base": fixture.head,
               "head_ref": "topic-child-two", "base_ref": "topic", "title": "Second child"},
        "99": {"head": "9999999999999999999999999999999999999999", "base": fixture.base,
               "head_ref": "unrelated", "base_ref": "trunk", "title": "Not a candidate"}
    }))?;
    fixture.configure("pulls_pages", json!([[19], [17, 99], [18]]))
}

struct NavigationRevision {
    head: String,
    parent: String,
    base: String,
    merge_base: String,
    base_files: BTreeMap<String, String>,
    head_files: BTreeMap<String, String>,
}

struct NavigationTransport {
    fixture: Fixture,
    revisions: BTreeMap<u64, NavigationRevision>,
}

fn snapshot_files(directory: &Path) -> Result<BTreeMap<String, String>> {
    fs::read_dir(directory)?
        .map(|entry| {
            let path = entry?.path();
            Ok((
                path.file_name()
                    .context("Missing snapshot filename")?
                    .to_str()
                    .context("Non-UTF-8 snapshot filename")?
                    .to_owned(),
                fs::read_to_string(path)?,
            ))
        })
        .collect()
}

impl NavigationTransport {
    fn new(colocate: bool, multiple_children: bool) -> Result<Self> {
        let open = OpenFixture::new(colocate)?;
        let fixture = open.fixture;
        let server = fixture.root.join("remote.git");
        let mut revisions = BTreeMap::from([(
            17,
            NavigationRevision {
                head: fixture.head.clone(),
                parent: open.first.clone(),
                base: fixture.base.clone(),
                merge_base: open.common,
                base_files: [(FILE, "one\ntwo\nthree\n"), ("removed.txt", "old\n")]
                    .map(|(name, text)| (name.to_owned(), text.to_owned()))
                    .into(),
                head_files: [
                    (FILE, "ONE\ntwo\nTHREE\n"),
                    ("early change.txt", "first commit\n"),
                    ("late change.txt", "second commit\n"),
                ]
                .map(|(name, text)| (name.to_owned(), text.to_owned()))
                .into(),
            },
        )]);
        let mut control = fixture.control()?;
        // The children target the parent's exact source repository and branch.
        // Their Git ancestry diverges at its first commit, before its latest head.
        control["head_repo"] = json!("acme/widgets");
        control["prs"] = json!({});
        let numbers = if multiple_children {
            vec![18, 19]
        } else {
            vec![18]
        };
        let mut transports = vec![(17, "main")];
        fixture.git_in(
            &server,
            &["update-ref", "refs/heads/topic", &fixture.head],
            "",
        )?;
        for number in &numbers {
            let text = format!("ONE\nchild {number}\nthree\n");
            let early = format!("First commit in PR #{number}\n");
            let late = format!("Last commit in PR #{number}\n");
            let mut files = vec![
                (FILE, text.as_str()),
                ("removed.txt", "old\n"),
                ("unchanged.txt", "same\n"),
                ("early change.txt", "first commit\n"),
                ("child early.txt", early.as_str()),
            ];
            let first = OpenFixture::commit(&fixture, &server, &files, Some(&open.first))?;
            files.push(("child late.txt", late.as_str()));
            let head = OpenFixture::commit(&fixture, &server, &files, Some(&first))?;
            fixture.git_in(
                &server,
                &["update-ref", &format!("refs/pull/{number}/head"), &head],
                "",
            )?;
            // The apparent source branch is deliberately not the PR head.
            fixture.git_in(
                &server,
                &[
                    "update-ref",
                    &format!("refs/heads/child-{number}"),
                    &fixture.base,
                ],
                "",
            )?;
            control["prs"][number.to_string()] = json!({
                "title": format!("Child PR {number}"), "head": head, "base": fixture.head,
                "head_ref": format!("child-{number}"), "base_ref": "topic",
                "files": [
                    {"filename": FILE, "status": "modified",
                     "patch": format!("@@ -1,3 +1,3 @@\n ONE\n-two\n+child {number}\n three")},
                    {"filename": "child early.txt", "status": "added",
                     "patch": format!("@@ -0,0 +1 @@\n+{}", early.trim_end())},
                    {"filename": "child late.txt", "status": "added",
                     "patch": format!("@@ -0,0 +1 @@\n+{}", late.trim_end())}
                ]
            });
            revisions.insert(
                *number,
                NavigationRevision {
                    head,
                    parent: first,
                    base: fixture.head.clone(),
                    merge_base: open.first.clone(),
                    base_files: [(FILE.to_owned(), "ONE\ntwo\nthree\n".to_owned())].into(),
                    head_files: [
                        (FILE.to_owned(), text),
                        ("child early.txt".to_owned(), early),
                        ("child late.txt".to_owned(), late),
                    ]
                    .into(),
                },
            );
            transports.push((*number, "topic"));
        }
        control["pulls_pages"] = if multiple_children {
            json!([[19], [17, 18]])
        } else {
            json!([[18], [17]])
        };
        fixture.set_control(control)?;
        install_local_pr_transport(&fixture, &transports)?;
        Ok(Self { fixture, revisions })
    }

    fn cleanup_assert_checkout(&self, number: u64) -> Result<PathBuf> {
        self.assert_checkout_kind(number, false)
    }

    fn assert_checkout_kind(&self, number: u64, immutable: bool) -> Result<PathBuf> {
        let fixture = &self.fixture;
        let revision = &self.revisions[&number];
        if immutable {
            fixture.assert_empty_child(&revision.head)?;
        } else {
            assert_eq!(
                fixture.sha("@")?,
                revision.head,
                "Edit the exact mutable PR head, not a child"
            );
            assert_eq!(fixture.sha("@-")?, revision.parent);
        }
        assert_eq!(
            fixture.git(&["merge-base", &revision.head, &revision.base])?,
            revision.merge_base
        );
        assert_ne!(
            revision.merge_base, revision.base,
            "Fixture must distinguish merge base from base tip"
        );
        let session = fixture.active_directory()?;
        let manifest = read_json(&session.join("manifest.json"))?;
        assert_eq!(manifest["pr"]["number"], number);
        assert_eq!(manifest["pr"]["head_sha"], revision.head);
        assert_eq!(manifest["pr"]["base_sha"], revision.base);
        assert_eq!(manifest["merge_base"], revision.merge_base);
        assert_eq!(manifest["directory"], json!(session));
        assert_eq!(snapshot_files(&session.join("base"))?, revision.base_files);
        assert_eq!(snapshot_files(&session.join("head"))?, revision.head_files);
        assert_eq!(
            fs::read_to_string(fixture.repo.join(FILE))?,
            revision.head_files[FILE]
        );
        let early = if number == 17 {
            "early change.txt"
        } else {
            "child early.txt"
        };
        assert!(
            !fixture
                .git(&["diff", "--name-only", &revision.parent, &revision.head])?
                .lines()
                .any(|path| path == early),
            "The early file must not be in the final commit's diff"
        );
        assert!(session.join("head").join(early).is_file());
        let receipt = read_json(&session.parent().unwrap().join("last-checkout.json"))?;
        assert_eq!(receipt["pr"], manifest["pr"]);
        assert_eq!(receipt["session"], json!(session));
        let launches = fixture.zed_calls()?;
        let launch = launches.last().context("Missing Zed diff launch")?;
        assert_eq!(launch["cwd"], json!(fixture.repo));
        assert_eq!(
            launch["args"],
            json!([
                "--existing",
                "--diff",
                session.join("base"),
                session.join("head")
            ])
        );
        assert_eq!(
            launch["active"],
            read_json(&fixture.state.join("active.json"))?
        );
        let relative = session
            .parent()
            .unwrap()
            .join("last-checkout.json")
            .strip_prefix(&fixture.state)?
            .to_str()
            .context("Non-UTF-8 receipt path")?
            .to_owned();
        assert_eq!(
            launch["checkouts"][relative], receipt,
            "Write receipt before launching Zed"
        );
        fixture.checkout_previous(&session)?;
        Ok(session)
    }

    fn assert_checkout(&self, number: u64) -> Result<PathBuf> {
        let session = self.cleanup_assert_checkout(number)?;
        assert!(
            self.fixture
                .calls()?
                .iter()
                .all(|call| call["method"] != "POST" && call["args"][0] != "pr")
        );
        assert!(self.fixture.editors()?.is_empty());
        Ok(session)
    }

    fn open_parent(&self) -> Result<PathBuf> {
        let fixture = &self.fixture;
        let bookmarks = fixture.previous_bookmarks()?;
        let output = fixture.run_default(&["--pr", PR_URL, "open"], "")?;
        success(&output);
        let session = self.assert_checkout(17)?;
        assert_eq!(fixture.previous_bookmarks()?, bookmarks);
        assert_return_command(&output, &fixture.checkout_previous(&session)?);
        Ok(session)
    }

    fn prepared_directory(&self, number: u64) -> Result<Option<PathBuf>> {
        for entry in fs::read_dir(&self.fixture.state)? {
            let pointer = entry?.path().join("prepared.json");
            if !pointer.is_file() {
                continue;
            }
            let pointer = read_json(&pointer)?;
            let directory = PathBuf::from(
                pointer["directory"]
                    .as_str()
                    .context("Missing prepared directory")?,
            );
            let manifest = read_json(&directory.join("manifest.json"))?;
            if manifest["pr"]["number"] == number
                && !directory.join("cleanup.json").exists()
                && !directory.join("cancelled.json").exists()
            {
                return Ok(Some(directory));
            }
        }
        Ok(None)
    }

    fn navigate(&self, action: &str, number: u64, side: &str, input: &str) -> Result<PathBuf> {
        let fixture = &self.fixture;
        let old = fixture.active_directory()?;
        let prepared = self.prepared_directory(number)?;
        let current_number = read_json(&old.join("manifest.json"))?["pr"]["number"]
            .as_u64()
            .context("Missing current PR number")?;
        let calls_before = fixture.calls()?.len();
        let fetches_before = self.fetch_numbers()?;
        let launches_before = fixture.zed_calls()?.len();
        let bookmarks_before = fixture.previous_bookmarks()?;
        let previous_change = fixture.jj(&[
            "--ignore-working-copy",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "change_id",
        ])?;
        let output = fixture.execute(
            navigation_command(fixture, &[action], Some(&old.join(side).join(FILE))),
            input,
        )?;
        success(&output);
        if action == "sf" && self.revisions.len() > 2 {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.contains("#18") && text.contains("#19") && text.contains("PR number"),
                "{text}"
            );
        }
        let session = self.assert_checkout(number)?;
        assert_ne!(session, old);
        assert_eq!(fixture.zed_calls()?.len(), launches_before + 1);
        let expected_fetches = if fetches_before.contains(&number) {
            vec![]
        } else {
            vec![number]
        };
        assert_eq!(
            &self.fetch_numbers()?[fetches_before.len()..],
            expected_fetches
        );
        assert_eq!(fixture.previous_bookmarks()?, bookmarks_before);
        let previous = fixture.checkout_previous(&session)?;
        assert_return_command(&output, &previous);
        assert_eq!(
            fixture.jj(&[
                "--ignore-working-copy",
                "log",
                "-r",
                &previous,
                "--no-graph",
                "-T",
                "change_id",
            ])?,
            previous_change,
            "Receipt must identify the checkout before navigation, including any snapshot"
        );
        let calls = fixture.calls()?;
        let calls = &calls[calls_before..];
        let files = calls.iter().position(|call| {
            call["endpoint"] == format!("repos/acme/widgets/pulls/{number}/files?per_page=100")
        });
        if let Some(prepared) = prepared {
            assert_eq!(
                session, prepared,
                "Reuse the intact prepared snapshot directory"
            );
            assert!(
                files.is_none(),
                "Cached snapshots must not request files again"
            );
        } else {
            assert!(
                files.is_some(),
                "A cache miss must prepare the full PR diff"
            );
        }
        for endpoint in [
            format!("repos/acme/widgets/pulls/{current_number}"),
            format!("repos/acme/widgets/pulls/{number}"),
            "user".to_owned(),
        ] {
            let reads: Vec<_> = calls
                .iter()
                .enumerate()
                .filter_map(|(index, call)| (call["endpoint"] == endpoint).then_some(index))
                .collect();
            assert!(
                reads.len() >= 2,
                "Recheck current/target PRs and reviewer: {endpoint}"
            );
            if let Some(files) = files {
                assert!(
                    reads[0] < files && *reads.last().unwrap() > files,
                    "Check {endpoint} both before and after preparing snapshots"
                );
            }
        }
        Ok(session)
    }

    fn fetch_numbers(&self) -> Result<Vec<u64>> {
        read_json_lines(&self.fixture.root.join("git.jsonl"))?
            .into_iter()
            .filter(|call| {
                call["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "fetch")
            })
            .map(|call| {
                let reference = call["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .find_map(|arg| arg.strip_prefix("refs/pull/"))
                    .context("Missing exact PR ref")?;
                Ok(reference.split('/').next().unwrap().parse()?)
            })
            .collect()
    }

    fn comment(&self, session: &Path, number: u64, body: &str) -> Result<PathBuf> {
        let file = if number == 17 {
            "early change.txt"
        } else {
            "child early.txt"
        };
        let mut command = self.fixture.default_cli(&["comment"]);
        command
            .env("ZED_FILE", session.join("head").join(file))
            .env(
                "ZED_SELECTED_TEXT",
                &self.revisions[&number].head_files[file],
            );
        success(&self.fixture.execute(command, body)?);
        let path = session.parent().unwrap().join("draft.json");
        let draft = read_json(&path)?;
        assert_eq!(draft["pr"]["number"], number);
        assert_eq!(draft["comments"][0]["path"], file);
        assert_eq!(draft["comments"][0]["body"], body);
        Ok(path)
    }

    fn assert_stale_tab(&self, old: &Path) -> Result<()> {
        let fixture = &self.fixture;
        let before = NavigationCheckpoint::capture(fixture)?;
        let fetches = self.fetch_numbers()?;
        let launches = fixture.zed_calls()?;
        for action in ["sb", "sf", "comment"] {
            let mut command = fixture.default_cli(&[action]);
            command
                .env("ZED_FILE", old.join("head").join(FILE))
                .env("ZED_SELECTED_TEXT", "ONE");
            let output = fixture.execute(command, "Do not save a stale-tab comment\n")?;
            navigation_failure(&output);
            assert!(
                describe(&output).to_ascii_lowercase().contains("head"),
                "{}",
                describe(&output)
            );
        }
        let after = NavigationCheckpoint::capture(fixture)?;
        assert_eq!(after.head, before.head);
        assert_eq!(after.active, before.active);
        assert_eq!(after.drafts, before.drafts);
        assert_eq!(self.fetch_numbers()?, fetches);
        assert_eq!(fixture.zed_calls()?, launches);
        Ok(())
    }
}

// Files, rather than pipes, keep a noisy or stuck child from deadlocking the test.
// stdin is a finite file, so comment input always ends with EOF, not a terminal.
fn bounded_output(command: Command, input: &str, directory: &Path) -> Result<Output> {
    let mut stdin = tempfile::tempfile_in(directory)?;
    stdin.write_all(input.as_bytes())?;
    stdin.seek(SeekFrom::Start(0))?;
    bounded_output_with_stdin(command, stdin, directory)
}

fn bounded_output_with_stdin(
    mut command: Command,
    stdin: File,
    directory: &Path,
) -> Result<Output> {
    let mut stdout = tempfile::tempfile_in(directory)?;
    let mut stderr = tempfile::tempfile_in(directory)?;
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    let description = format!("{command:?}");
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot start {description}"))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut output = Output {
        status,
        stdout: vec![],
        stderr: vec![],
    };
    stdout.read_to_end(&mut output.stdout)?;
    stderr.read_to_end(&mut output.stderr)?;
    if timed_out {
        bail!(
            "Child exceeded 20 seconds: {description}\n{}",
            describe(&output)
        );
    }
    Ok(output)
}

fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn archives(directory: &Path) -> Result<Vec<Value>> {
    let mut drafts = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("archive-")
        {
            drafts.push(read_json(&path)?);
        }
    }
    Ok(drafts)
}

fn read_json_lines(path: &Path) -> Result<Vec<Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    text.lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn assert_review_template(editor: &Value, comments: &Value) {
    let initial = editor["initial"].as_str().unwrap();
    let reference = initial
        .strip_prefix("\n\n")
        .expect("The review template must start with two blank lines");
    let lines: Vec<&str> = reference
        .lines()
        .map(|line| {
            let text = line
                .strip_prefix("#|")
                .expect("Every reference line must start with #|");
            text.strip_prefix(' ').unwrap_or(text)
        })
        .collect();
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (line.contains(FILE) && line.contains("RIGHT")).then_some(index)
        })
        .collect();
    let comments = comments.as_array().unwrap();
    assert!(!comments.is_empty());
    assert_eq!(headers.len(), comments.len(), "{initial}");
    assert!(
        lines[..headers[0]]
            .iter()
            .any(|line| !line.trim().is_empty()),
        "Instructions must precede the comments: {initial}"
    );
    for (index, comment) in comments.iter().enumerate() {
        let header = lines[headers[index]];
        let path = comment["path"].as_str().unwrap();
        let (_, coordinates) = header.split_once(path).unwrap();
        let coordinates: Vec<u64> = coordinates
            .split(|character: char| !character.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .map(|part| part.parse().unwrap())
            .collect();
        let start = comment["start_line"].as_u64().unwrap();
        let end = comment["line"].as_u64().unwrap();
        assert_eq!(coordinates, vec![start, end], "{header}");

        let section_end = headers.get(index + 1).copied().unwrap_or(lines.len());
        let section = &lines[headers[index] + 1..section_end];
        let body: Vec<&str> = comment["body"].as_str().unwrap().lines().collect();
        let body_start = section
            .windows(body.len())
            .position(|window| window == body.as_slice())
            .expect("The saved multiline comment body must appear unchanged after the source");
        let source: Vec<(bool, u64, &str)> = section[..body_start]
            .iter()
            .filter_map(|line| {
                let (gutter, text) = line.split_once('|')?;
                let gutter = gutter.trim();
                let selected = gutter.starts_with('>');
                let number = gutter.strip_prefix('>').unwrap_or(gutter).trim();
                Some((selected, number.parse().ok()?, text.trim()))
            })
            .collect();
        let expected: Vec<_> = [(1, "one"), (2, "two"), (3, "three")]
            .into_iter()
            .map(|(number, text)| ((start..=end).contains(&number), number, text))
            .collect();
        assert_eq!(source, expected, "{initial}");
    }
}

fn assert_ready_api_post(call: &Value, host: &str) {
    let args = call["args"].as_array().unwrap();
    assert_eq!(args[0], "api");
    assert_eq!(call["method"], "POST");
    assert_eq!(call["hostname"], host);
    for (flag, value) in [
        ("--hostname", host),
        ("--method", "POST"),
        ("--input", "-"),
        ("--header", "Accept: application/vnd.github+json"),
        ("--header", "X-GitHub-Api-Version: 2022-11-28"),
    ] {
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value),
            "Missing {flag} {value}: {call}"
        );
    }
}

fn assert_ready_checks(calls: &[Value], minimum: usize) {
    for endpoint in [PR_ENDPOINT, "user"] {
        let count = calls
            .iter()
            .filter(|call| call["method"] == "GET" && call["endpoint"] == endpoint)
            .count();
        assert!(
            count >= minimum,
            "Expected at least {minimum} GET {endpoint} calls: {calls:?}"
        );
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_return_command(output: &Output, previous: &str) {
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!("Return with: jj edit {previous}")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "If that revision is immutable, use: jj new {previous}"
        )),
        "{text}"
    );
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", describe(output));
}

fn failure(output: &Output) {
    assert!(
        !output.status.success(),
        "Unexpected success: {}",
        describe(output)
    );
}

#[test]
fn navigation_local_without_session_uses_jj_aliases_and_watch_is_one_shot_without_a_tty()
-> Result<()> {
    let fixture = navigation_fixture()?;
    fixture.configure("deny_discovery", json!(true))?;
    for file in [
        None,
        Some(Path::new("")),
        Some(fixture.repo.join(FILE).as_path()),
    ] {
        for (args, alias, expected) in [
            (&["stack"][..], "l", &fixture.head),
            (&["stack", "--watch"][..], "l", &fixture.head),
            (&["sb"][..], "sb", &fixture.base),
            (&["sf"][..], "sf", &fixture.head),
        ] {
            navigation_clear_calls(&fixture)?;
            // The fixture redirects stdin and stdout, so --watch must not open a TUI.
            let output = fixture.execute(navigation_command(&fixture, args, file), "")?;
            success(&output);
            if alias == "l" {
                assert!(String::from_utf8_lossy(&output.stdout).contains("Navigation PR head"));
            }
            navigation_assert_local_alias(&fixture, alias)?;
            assert_eq!(&fixture.sha("@")?, expected);
            assert!(
                !fixture.state.exists(),
                "Normal navigation must not create review state"
            );
        }
    }
    Ok(())
}

#[test]
fn navigation_source_context_ignores_active_review_even_if_its_pointer_is_invalid() -> Result<()> {
    let (fixture, _) = navigation_session()?;
    let draft = fs::read(fixture.draft_path()?)?;
    fixture.configure("deny_discovery", json!(true))?;
    for invalid in [false, true] {
        if invalid {
            fs::write(fixture.state.join("active.json"), "not valid JSON\n")?;
        }
        let active = fs::read(fixture.state.join("active.json"))?;
        for args in [&["stack"][..], &["stack", "--watch"], &["sb"], &["sf"]] {
            navigation_clear_calls(&fixture)?;
            success(&fixture.run_default(args, "")?);
            let alias = if args[0] == "stack" { "l" } else { args[0] };
            navigation_assert_local_alias(&fixture, alias)?;
        }
        assert_eq!(fixture.sha("@")?, fixture.head);
        assert!(fixture.calls()?.is_empty());
        assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
        assert_eq!(fs::read(fixture.draft_path()?)?, draft);
        assert!(fixture.zed_calls()?.is_empty());
    }
    Ok(())
}

#[test]
fn navigation_missing_or_empty_context_uses_active_review_only_at_its_exact_head() -> Result<()> {
    let (fixture, _) = navigation_session()?;
    // The recorded @, not an implicit snapshot of pending edits, chooses the mode.
    fs::write(fixture.repo.join(FILE), "one\ntwo\nunsnapshotted edit\n")?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for file in [None, Some(Path::new(""))] {
        for args in [&["stack"][..], &["stack", "--watch"], &["sb"], &["sf"]] {
            let start = fixture.calls()?.len();
            let output = fixture.execute(navigation_command(&fixture, args, file), "")?;
            success(&output);
            let endpoint = if args[0] == "stack" {
                PULLS_ENDPOINT
            } else {
                PR_ENDPOINT
            };
            assert!(
                fixture.calls()?[start..]
                    .iter()
                    .any(|call| call["endpoint"] == endpoint)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("@ #17"));
        }
    }
    before.assert_unchanged(&fixture)?;
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "one\ntwo\nunsnapshotted edit\n"
    );
    assert!(fixture.calls()?.iter().all(|call| call["args"][0] != "pr"));
    Ok(())
}

#[test]
fn navigation_empty_immutable_head_child_keeps_review_mode_in_zed_and_terminal() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let child = fixture.empty_immutable_head_child()?;
    for keep_immutable in [true, false] {
        if !keep_immutable {
            fixture.set_immutable_heads(&[])?;
        }
        let before = NavigationCheckpoint::capture(&fixture)?;
        navigation_clear_calls(&fixture)?;
        for file in [
            Some(session.join("head").join(FILE)),
            Some(session.join("base").join(FILE)),
            None,
            Some(PathBuf::new()),
        ] {
            for args in [&["stack"][..], &["stack", "--watch"], &["sb"], &["sf"]] {
                let start = fixture.calls()?.len();
                let output =
                    fixture.execute(navigation_command(&fixture, args, file.as_deref()), "")?;
                success(&output);
                let endpoint = if args[0] == "stack" {
                    PULLS_ENDPOINT
                } else {
                    PR_ENDPOINT
                };
                assert!(
                    fixture.calls()?[start..]
                        .iter()
                        .any(|call| call["endpoint"] == endpoint),
                    "Expected review navigation for {args:?}, file {file:?}: {}",
                    describe(&output)
                );
                if args[0] == "stack" {
                    assert!(String::from_utf8_lossy(&output.stdout).contains("@ #17"));
                }
            }
        }
        before.assert_unchanged(&fixture)?;
        assert_eq!(fixture.assert_empty_child(&fixture.head)?, child);
    }
    Ok(())
}

#[test]
fn navigation_missing_or_empty_context_away_from_active_head_stays_local() -> Result<()> {
    let (fixture, _) = navigation_session()?;
    let active = fs::read(fixture.state.join("active.json"))?;
    let draft = fs::read(fixture.draft_path()?)?;
    let away = fixture.nonempty_child("Unrelated local work")?;
    fixture.jj(&["new", "-m", "Local child"])?;
    let child = fixture.sha("@")?;
    fixture.configure("deny_discovery", json!(true))?;
    for file in [None, Some(Path::new(""))] {
        for (args, expected) in [
            (&["stack"][..], &away),
            (&["stack", "--watch"][..], &away),
            (&["sb"][..], &fixture.head),
            (&["sf"][..], &child),
        ] {
            fixture.jj(&["edit", &away])?;
            navigation_clear_calls(&fixture)?;
            success(&fixture.execute(navigation_command(&fixture, args, file), "")?);
            let alias = if args[0] == "stack" { "l" } else { args[0] };
            navigation_assert_local_alias(&fixture, alias)?;
            assert_eq!(&fixture.sha("@")?, expected);
            assert!(fixture.calls()?.is_empty());
            assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
            assert_eq!(fs::read(fixture.draft_path()?)?, draft);
        }
    }
    Ok(())
}

#[test]
fn navigation_registered_head_and_base_list_the_paginated_stack_without_mutation() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    navigation_children(&fixture)?;
    let mut control = fixture.control()?;
    control["prs"]["16"] = json!({
        "head": fixture.base, "base": "1616161616161616161616161616161616161616",
        "head_ref": "main", "base_ref": "trunk", "title": "Stack parent", "draft": false
    });
    control["pulls_pages"] = json!([[19], [16, 17, 99], [18]]);
    fixture.set_control(control)?;
    fs::write(
        fixture.repo.join(FILE),
        "uncommitted work must stay untouched\n",
    )?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for side in ["head", "base"] {
        for args in [&["stack"][..], &["stack", "--watch"]] {
            let file = session.join(side).join(FILE);
            let output = fixture.execute(navigation_command(&fixture, args, Some(&file)), "")?;
            success(&output);
            let text = String::from_utf8_lossy(&output.stdout);
            for expected in [
                "#16",
                "#17",
                "#18",
                "#19",
                "Stack parent",
                "First child",
                "Second child",
                "#17 topic [draft]",
                "#16 main",
                "◆ trunk (base)",
                "[ready]",
                "current",
                PR_URL,
            ] {
                assert!(text.contains(expected), "Missing {expected:?}: {text}");
            }
            assert!(text.contains("@ #17"), "{text}");
            assert!(
                !text.contains('\u{1b}'),
                "Redirected output must be plain: {text:?}"
            );
            assert_eq!(text.matches("(base)").count(), 1, "{text}");
            let lines: Vec<_> = text.lines().collect();
            let current = lines
                .iter()
                .position(|line| line.starts_with("@ #17 "))
                .unwrap();
            assert_eq!(lines[current + 1], "│ Offline review fixture");
            assert_eq!(lines[current + 2], format!("│ {PR_URL}"));
            assert!(
                !text.contains("#99"),
                "Unrelated PR is not in the stack: {text}"
            );
            assert!(text.find("#18") < text.find("#19"));
            assert!(text.find("#19") < text.find("#17"));
            assert!(text.find("#17") < text.find("#16"));
        }
    }
    before.assert_unchanged(&fixture)?;
    let calls = fixture.calls()?;
    assert!(calls.iter().all(|call| call["args"][0] != "pr"));
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["endpoint"] == PULLS_ENDPOINT)
            .count(),
        4
    );
    for number in [16, 17, 18, 19] {
        assert!(
            calls
                .iter()
                .any(|call| call["endpoint"] == format!("repos/acme/widgets/pulls/{number}"))
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "uncommitted work must stay untouched\n"
    );
    Ok(())
}

// Reuse the strict navigation transport and a real saved draft, but list enough
// PRs to put the current review below the first page of a ten-row terminal.
fn stack_terminal_fixture() -> Result<(Fixture, PathBuf)> {
    let (fixture, session) = navigation_session()?;
    let mut control = fixture.control()?;
    let mut prs = serde_json::Map::new();
    prs.insert(
        "16".into(),
        json!({
            "head": fixture.base, "base": format!("{:040x}", 16),
            "head_ref": "main", "base_ref": "trunk",
            "title": "Stack level 16", "draft": false
        }),
    );
    let mut parent_head = fixture.head.clone();
    let mut parent_branch = String::from("topic");
    for number in 18..=23 {
        let head = format!("{number:040x}");
        let branch = format!("topic-{number}");
        prs.insert(
            number.to_string(),
            json!({
                "head": head, "base": parent_head,
                "head_ref": branch, "base_ref": parent_branch,
                "title": format!("Stack level {number}"), "draft": false
            }),
        );
        parent_head = head;
        parent_branch = branch;
    }
    control["prs"] = Value::Object(prs);
    control["pulls_pages"] = json!([[20, 16, 23], [18, 21, 17], [22, 19]]);
    fixture.set_control(control)?;
    Ok((fixture, session))
}

fn run_stack_terminal_test(scenario: &str) -> Result<()> {
    let (fixture, session) = stack_terminal_fixture()?;
    if matches!(scenario, "active-review" | "initial-active-review") {
        // Two real sessions share the same local commits, but belong to separate
        // PR stacks. Switching only the active pointer isolates viewer following
        // from checkout/transport mutations and keeps the read-only checkpoint.
        let mut control = fixture.control()?;
        control["prs"]["24"] = json!({
            "head": fixture.head, "base": fixture.base,
            "head_ref": "alternate-topic", "base_ref": "alternate-main",
            "title": "New active review", "draft": false
        });
        control["pulls_pages"]
            .as_array_mut()
            .unwrap()
            .push(json!([24]));
        fixture.set_control(control)?;
        let active = fixture.state.join("active.json");
        let original = fs::read(&active)?;
        let mut prepare = fixture.command(std::env::current_exe()?);
        prepare
            .args(["--exact", "session_fixture_child", "--nocapture"])
            .env("ZPR_PREPARE_SESSION", "1")
            .env(
                "ZPR_PREPARE_SESSION_PR",
                "https://github.com/acme/widgets/pull/24",
            );
        success(&fixture.execute(prepare, "")?);
        fs::copy(&active, fixture.root.join("stack-next-active.json"))?;
        fs::write(active, original)?;
    }
    let work = "unsnapshotted work must survive the PR viewer\n";
    fs::write(fixture.repo.join(FILE), work)?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    let bookmarks = fixture.jj(&["--ignore-working-copy", "bookmark", "list", "--all-remotes"])?;
    let refs = fixture.git(&["for-each-ref", "--format=%(refname) %(objectname)"])?;
    navigation_clear_calls(&fixture)?;

    let transcript = fixture.root.join("stack-terminal-output.txt");
    let output = File::create(&transcript)?;
    // This environment is constructed by Fixture, not inherited from the test
    // runner. The Rust driver and viewer must keep the offline gh/Git/jj shims.
    let mut command = fixture.command(env!("CARGO_BIN_EXE_zpr-test-helper"));
    command
        .arg("stack-terminal")
        .arg(env!("CARGO_BIN_EXE_zed-pr-review"))
        .arg(scenario)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    match scenario {
        "escape" => {
            command.env_remove("ZED_FILE");
        }
        "ctrl-c" => {
            command.env("ZED_FILE", session.join("base").join(FILE));
        }
        _ => {
            command.env("ZED_FILE", session.join("head").join(FILE));
        }
    }
    let mut child = command
        .spawn()
        .context("Cannot start the Rust PR stack PTY driver")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // The Rust driver creates a session for the viewer and its transport children.
    // Clean up the whole group even if an assertion or timeout interrupts a read.
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = fs::read_to_string(transcript)?;
    ensure!(
        status.is_some(),
        "Rust PR stack PTY driver {scenario} exceeded 60 seconds:\n{output}"
    );
    ensure!(
        status.unwrap().success(),
        "Rust PR stack PTY driver {scenario} failed:\n{output}"
    );
    for call in fixture.calls()? {
        ensure!(
            call.get("unexpected").is_none(),
            "Offline gh rejected: {call}"
        );
    }
    before.assert_unchanged(&fixture)?;
    assert_eq!(fs::read_to_string(fixture.repo.join(FILE))?, work);
    assert_eq!(
        fixture.jj(&["--ignore-working-copy", "bookmark", "list", "--all-remotes"])?,
        bookmarks
    );
    assert_eq!(
        fixture.git(&["for-each-ref", "--format=%(refname) %(objectname)"])?,
        refs
    );
    assert!(
        fixture
            .calls()?
            .iter()
            .any(|call| call["endpoint"] == PULLS_ENDPOINT)
    );
    Ok(())
}

#[test]
fn stack_terminal_navigation_refresh_and_quit_are_read_only() -> Result<()> {
    run_stack_terminal_test("navigation")
}

#[test]
fn stack_terminal_coalesces_refresh_requests_while_reading() -> Result<()> {
    run_stack_terminal_test("queued-refresh")
}

#[test]
fn stack_terminal_follows_active_review_after_initial_discovery_failure() -> Result<()> {
    run_stack_terminal_test("initial-active-review")
}

#[test]
fn stack_terminal_automatic_refresh_updates_title_and_draft_without_input() -> Result<()> {
    run_stack_terminal_test("automatic-refresh")
}

#[test]
fn stack_terminal_follows_changed_active_review_without_rebinding_original_diff() -> Result<()> {
    run_stack_terminal_test("active-review")
}

#[test]
fn stack_terminal_ctrl_c_restores_the_terminal() -> Result<()> {
    run_stack_terminal_test("ctrl-c")
}

#[test]
fn stack_terminal_escape_restores_the_terminal_in_active_review_context() -> Result<()> {
    run_stack_terminal_test("escape")
}

#[test]
fn stack_terminal_stdout_only_tty_with_piped_stdin_is_one_shot() -> Result<()> {
    run_stack_terminal_test("stdin-pipe")
}

#[test]
fn navigation_cached_stack_discovers_new_children_only_after_full_refresh() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    let file = session.join("head").join(FILE);
    success(&fixture.execute(navigation_command(&fixture, &["stack"], Some(&file)), "")?);
    navigation_children(&fixture)?;
    navigation_clear_calls(&fixture)?;
    let output = fixture.execute(navigation_command(&fixture, &["sf"], Some(&file)), "")?;
    success(&output);
    assert!(describe(&output).contains("No open child PR"));
    assert!(
        fixture
            .calls()?
            .iter()
            .any(|call| call["endpoint"] == PR_ENDPOINT)
    );
    assert!(
        fixture
            .calls()?
            .iter()
            .all(|call| call["endpoint"] != PULLS_ENDPOINT)
    );
    let output = fixture.execute(navigation_command(&fixture, &["stack"], Some(&file)), "")?;
    success(&output);
    assert!(describe(&output).contains("#18") && describe(&output).contains("#19"));
    assert!(
        fixture
            .calls()?
            .iter()
            .any(|call| call["endpoint"] == PULLS_ENDPOINT)
    );
    navigation_clear_calls(&fixture)?;
    let output = fixture.execute(navigation_command(&fixture, &["sf"], Some(&file)), "\n")?;
    success(&output);
    assert!(describe(&output).contains("PR number") && describe(&output).contains("Cancelled"));
    assert!(
        fixture
            .calls()?
            .iter()
            .all(|call| call["endpoint"] != PULLS_ENDPOINT)
    );
    before.assert_unchanged(&fixture)?;
    Ok(())
}

#[test]
fn navigation_boundaries_report_no_parent_or_child_and_keep_drafts() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for side in ["head", "base"] {
        let file = session.join(side).join(FILE);
        for (action, neighbor) in [("sb", "parent"), ("sf", "child")] {
            let output =
                fixture.execute(navigation_command(&fixture, &[action], Some(&file)), "")?;
            success(&output);
            let text = describe(&output).to_ascii_lowercase();
            assert!(text.contains(neighbor) && text.contains("no"), "{text}");
            before.assert_unchanged(&fixture)?;
        }
    }
    Ok(())
}

#[test]
fn navigation_ambiguous_children_cancel_on_a_blank_first_line() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    navigation_children(&fixture)?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for input in ["", "\n18\n", " \t\n19\n"] {
        let output = fixture.execute(
            navigation_command(&fixture, &["sf"], Some(&session.join("base").join(FILE))),
            input,
        )?;
        success(&output);
        let text = describe(&output).to_ascii_lowercase();
        assert!(text.contains("#18") && text.contains("#19"), "{text}");
        assert!(text.contains("number") && text.contains("cancel"), "{text}");
        assert!(!text.contains("#99"), "{text}");
        before.assert_unchanged(&fixture)?;
    }
    Ok(())
}

#[test]
fn navigation_ambiguous_children_reject_choices_outside_offered_pr_numbers() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    navigation_children(&fixture)?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for input in [
        "1\n",
        "2\n",
        "17\n",
        "99\n",
        "0\n",
        "-1\n",
        "18 19\n",
        "https://github.com/acme/widgets/pull/18\n",
        "18446744073709551616\n",
        "wrong\n18\n",
    ] {
        let output = fixture.execute(
            navigation_command(&fixture, &["sf"], Some(&session.join("head").join(FILE))),
            input,
        )?;
        failure(&output);
        let text = describe(&output);
        assert!(
            text.contains("#18") && text.contains("#19"),
            "Must reach candidate selection: {text}"
        );
        before.assert_unchanged(&fixture)?;
    }
    assert!(
        fixture
            .calls()?
            .iter()
            .all(|call| call["endpoint"] != "repos/acme/widgets/pulls/99")
    );
    Ok(())
}

#[test]
fn navigation_invalid_state_selections_never_fall_back_to_jj() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let unregistered = session.join("head/unregistered.txt");
    fs::write(&unregistered, "one\ntwo\nTHREE\n")?;
    let unknown = fixture.state.join("unknown.txt");
    fs::write(&unknown, "one\ntwo\nTHREE\n")?;
    let alias = fixture.root.join("snapshot-alias.txt");
    symlink(session.join("head").join(FILE), &alias)?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for file in [
        session.join("manifest.json"),
        fixture.state.join("active.json"),
        fixture.draft_path()?,
        unknown,
        unregistered,
        alias,
        session.join("head/missing.txt"),
    ] {
        for action in ["stack", "sb", "sf"] {
            navigation_failure(
                &fixture.execute(navigation_command(&fixture, &[action], Some(&file)), "")?,
            );
            before.assert_unchanged(&fixture)?;
        }
    }
    assert!(
        fixture.calls()?.is_empty(),
        "Invalid local state must be refused before gh"
    );
    Ok(())
}

#[test]
fn navigation_tampered_snapshots_and_manifests_never_fall_back_to_jj() -> Result<()> {
    for tampered in ["head", "base", "manifest.json"] {
        let (fixture, session) = navigation_session()?;
        let path = if tampered == "manifest.json" {
            session.join(tampered)
        } else {
            session.join(tampered).join(FILE)
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        fs::write(&path, "one\ntwo\naltered data\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
        let before = NavigationCheckpoint::capture(&fixture)?;
        navigation_clear_calls(&fixture)?;
        for side in ["head", "base"] {
            for action in ["stack", "sb", "sf"] {
                navigation_failure(&fixture.execute(
                    navigation_command(&fixture, &[action], Some(&session.join(side).join(FILE))),
                    "",
                )?);
                before.assert_unchanged(&fixture)?;
            }
        }
        assert!(fixture.calls()?.is_empty());
    }
    Ok(())
}

#[test]
fn navigation_missing_context_rejects_an_invalid_active_pointer_without_fallback() -> Result<()> {
    let (fixture, _) = navigation_session()?;
    fs::write(
        fixture.state.join("active.json"),
        "invalid active session\n",
    )?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for file in [None, Some(Path::new(""))] {
        for action in ["stack", "sb", "sf"] {
            navigation_failure(
                &fixture.execute(navigation_command(&fixture, &[action], file), "")?,
            );
            before.assert_unchanged(&fixture)?;
        }
    }
    assert!(fixture.calls()?.is_empty());
    Ok(())
}

#[test]
fn navigation_stale_selected_snapshot_rejects_checkout_and_remote_head_drift() -> Result<()> {
    for remote_drift in [false, true] {
        let (fixture, session) = navigation_session()?;
        if remote_drift {
            fixture.configure("head", json!(fixture.base))?;
        } else {
            fixture.nonempty_child("Unrelated local checkout")?;
        }
        let before = NavigationCheckpoint::capture(&fixture)?;
        navigation_clear_calls(&fixture)?;
        for side in ["head", "base"] {
            for action in ["stack", "sb", "sf"] {
                let output = fixture.execute(
                    navigation_command(&fixture, &[action], Some(&session.join(side).join(FILE))),
                    "",
                )?;
                failure(&output);
                let text = describe(&output).to_ascii_lowercase();
                assert!(text.contains("head") || text.contains("session"), "{text}");
                before.assert_unchanged(&fixture)?;
            }
        }
    }
    Ok(())
}

#[test]
fn navigation_old_snapshot_is_not_rebound_to_a_new_active_session() -> Result<()> {
    let (mut fixture, old) = navigation_session()?;
    let old_manifest = fs::read(old.join("manifest.json"))?;
    fixture.jj(&["new", "-m", "New PR revision"])?;
    fixture.head = fixture.sha("@")?;
    fixture.configure("head", json!(fixture.head))?;
    let current = fixture.prepare_session()?;
    assert_ne!(old, current);
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for side in ["head", "base"] {
        for action in ["stack", "sb", "sf"] {
            navigation_failure(&fixture.execute(
                navigation_command(&fixture, &[action], Some(&old.join(side).join(FILE))),
                "",
            )?);
            before.assert_unchanged(&fixture)?;
        }
    }
    assert_eq!(fixture.active_directory()?, current);
    assert_eq!(fs::read(old.join("manifest.json"))?, old_manifest);
    Ok(())
}

#[test]
fn navigation_rejects_draft_dir_before_any_discovery_or_local_fallback() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for file in [
        None,
        Some(fixture.repo.join(FILE)),
        Some(session.join("head").join(FILE)),
    ] {
        for args in [&["stack"][..], &["stack", "--watch"], &["sb"], &["sf"]] {
            let mut command = navigation_command(&fixture, args, file.as_deref());
            command
                .arg("--draft-dir")
                .arg(fixture.draft_path()?.parent().unwrap());
            let output = fixture.execute(command, "")?;
            failure(&output);
            assert!(describe(&output).contains("--draft-dir"));
            before.assert_unchanged(&fixture)?;
        }
    }
    assert!(fixture.calls()?.is_empty());
    Ok(())
}

#[test]
fn navigation_stack_rejects_a_pr_change_during_discovery() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    let next = fixture.control()?["pr_reads"].as_u64().unwrap() + 3;
    // discover(@), Stack::discover(before), then the post-listing identity check.
    fixture.configure(
        "pr_read_changes",
        json!({ next.to_string(): {"base_ref": "changed-base"} }),
    )?;
    navigation_clear_calls(&fixture)?;
    let output = fixture.execute(
        navigation_command(&fixture, &["stack"], Some(&session.join("head").join(FILE))),
        "",
    )?;
    failure(&output);
    assert!(describe(&output).to_ascii_lowercase().contains("changed"));
    assert!(
        fixture
            .calls()?
            .iter()
            .any(|call| call["endpoint"] == PULLS_ENDPOINT)
    );
    before.assert_unchanged(&fixture)?;
    Ok(())
}

#[test]
fn navigation_adjacent_prs_enter_the_shared_open_flow_with_the_exact_pr_ref() -> Result<()> {
    for (action, multiple, input, expected, base_ref) in [
        ("sb", false, "not a choice\n", 16, "trunk"),
        ("sf", false, "not a choice\n", 18, "topic"),
        ("sf", true, "19\n18\n", 19, "topic"),
    ] {
        let (fixture, session) = navigation_session()?;
        navigation_children(&fixture)?;
        let mut control = fixture.control()?;
        control["prs"]["16"] = json!({
            "head": fixture.base, "base": fixture.sha("@--")?,
            "head_ref": "main", "base_ref": "trunk", "title": "Stack parent"
        });
        control["pulls_pages"] = if multiple {
            json!([[16, 17], [18, 19]])
        } else {
            json!([[16, 17, 18]])
        };
        fixture.set_control(control)?;
        let before = NavigationCheckpoint::capture(&fixture)?;
        navigation_clear_calls(&fixture)?;
        let output = fixture.execute(
            navigation_command(&fixture, &[action], Some(&session.join("base").join(FILE))),
            input,
        )?;
        // The strict shim refuses this fetch before any transport can run. This
        // proves selection reaches open, without a second end-to-end Git server.
        navigation_failure(&output);
        let calls = navigation_calls(&fixture)?;
        let fetches: Vec<_> = calls
            .iter()
            .filter(|call| {
                call["program"] == "git"
                    && call["args"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|arg| arg == "fetch")
            })
            .collect();
        assert_eq!(
            fetches.len(),
            1,
            "Expected one refused fetch: {}\n{calls:?}",
            describe(&output)
        );
        let args: Vec<_> = fetches[0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"https://github.com/acme/widgets.git"));
        assert!(args.iter().any(|arg| arg.starts_with(&format!(
            "refs/pull/{expected}/head:refs/heads/zed-pr-review/"
        ))));
        assert!(args.iter().any(|arg| {
            arg.starts_with(&format!("refs/heads/{base_ref}:refs/heads/zed-pr-review/"))
        }));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("refs/pull/"))
                .count(),
            1
        );
        assert!(
            fixture
                .calls()?
                .iter()
                .filter(|call| call["endpoint"] == PULLS_ENDPOINT)
                .count()
                >= 2,
            "Verify adjacency again after choosing, before open"
        );
        before.assert_preserved(&fixture)?;
        for call in calls {
            if call["program"] == "jj" {
                assert!(!call["args"].as_array().unwrap().iter().any(|arg| {
                    ["edit", "new", "sb", "sf"]
                        .iter()
                        .any(|forbidden| arg == forbidden)
                }));
            }
        }
    }
    Ok(())
}

#[test]
fn navigation_selected_child_is_revalidated_before_fetching() -> Result<()> {
    for change in ["head", "base_ref"] {
        let (fixture, session) = navigation_session()?;
        navigation_children(&fixture)?;
        let before = NavigationCheckpoint::capture(&fixture)?;
        let mut changed = fixture.control()?["prs"].clone();
        changed["18"][change] = if change == "head" {
            json!(fixture.base)
        } else {
            json!("unrelated-base")
        };
        // Current discovery and the stack's before/listed checks read 17 three
        // times, then 18 and 19. The sixth read refreshes the chosen PR.
        let next = fixture.control()?["pr_reads"].as_u64().unwrap() + 6;
        fixture.configure(
            "pr_read_changes",
            json!({ next.to_string(): {"prs": changed} }),
        )?;
        navigation_clear_calls(&fixture)?;
        let output = fixture.execute(
            navigation_command(&fixture, &["sf"], Some(&session.join("head").join(FILE))),
            "18\n19\n",
        )?;
        failure(&output);
        let text = describe(&output).to_ascii_lowercase();
        assert!(text.contains("#18") && text.contains("#19"), "{text}");
        assert!(
            text.contains("changed") || text.contains("relationship"),
            "{text}"
        );
        assert!(fixture.control()?["pr_reads"].as_u64().unwrap() >= next);
        before.assert_unchanged(&fixture)?;
    }
    Ok(())
}

fn navigation_transport_round_trip(colocate: bool) -> Result<()> {
    let transport = NavigationTransport::new(colocate, false)?;
    let fixture = &transport.fixture;
    let original_checkout = fixture.sha("@")?;
    // Leave an old preservation bookmark alone, without snapshotting the pending work.
    fixture.jj(&[
        "--ignore-working-copy",
        "bookmark",
        "create",
        "zed-pr-review/previous-legacy",
        "-r",
        "@-",
    ])?;
    let legacy_bookmarks = fixture.previous_bookmarks()?;
    assert_eq!(legacy_bookmarks.len(), 1);
    let parent = transport.open_parent()?;
    let local_work = fixture.checkout_previous(&parent)?;
    assert_ne!(
        local_work, original_checkout,
        "Preserve edits present before the first open"
    );
    assert_eq!(fixture.sha("unrelated-local-work")?, local_work);
    assert_eq!(
        fixture.git(&["show", &format!("{local_work}:keep local.txt")])?,
        "Unsnapshotted work"
    );
    let parent_body = "Keep the parent PR's private comment";
    let child_body = "Keep the child PR's separate private comment";
    let parent_draft = transport.comment(&parent, 17, parent_body)?;
    let parent_bytes = fs::read(&parent_draft)?;
    let old_manifest = fs::read(parent.join("manifest.json"))?;
    let old_base = snapshot_files(&parent.join("base"))?;
    let old_head = snapshot_files(&parent.join("head"))?;
    fixture.jj(&["bookmark", "create", "keep-parent-work"])?;
    fs::write(fixture.repo.join(FILE), "ONE\nlocal parent edit\nTHREE\n")?;
    fs::write(
        fixture.repo.join("parent local.txt"),
        "Unsnapshotted work while reviewing the parent\n",
    )?;
    assert_eq!(fixture.sha("@")?, transport.revisions[&17].head);

    let child = transport.navigate("sf", 18, "base", "single child needs no choice\n")?;
    let parent_work = fixture.checkout_previous(&child)?;
    assert_ne!(
        parent_work, transport.revisions[&17].head,
        "Snapshot the parent's pending edits before switching"
    );
    assert_eq!(fixture.sha("keep-parent-work")?, parent_work);
    assert_eq!(
        fixture.git(&["show", &format!("{parent_work}:{FILE}")])?,
        "ONE\nlocal parent edit\nTHREE"
    );
    assert_eq!(
        fixture.git(&["show", &format!("{parent_work}:parent local.txt")])?,
        "Unsnapshotted work while reviewing the parent"
    );
    assert!(
        !fixture
            .git(&["ls-tree", "-r", "--name-only", &parent_work])?
            .contains("scratch/zed-pr-review")
    );
    assert!(!fixture.repo.join("parent local.txt").exists());
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    let child_draft = transport.comment(&child, 18, child_body)?;
    let child_bytes = fs::read(&child_draft)?;
    assert_ne!(parent_draft.parent(), child_draft.parent());
    assert_eq!(fixture.drafts()?.len(), 2);
    fixture.jj(&["bookmark", "create", "keep-child-head"])?;
    transport.assert_stale_tab(&parent)?;

    let returned_parent = transport.navigate("sb", 17, "head", "")?;
    assert_eq!(
        fixture.checkout_previous(&returned_parent)?,
        transport.revisions[&18].head
    );
    assert_eq!(
        returned_parent, parent,
        "Returning must reuse the intact prepared session and its PR draft"
    );
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    assert_eq!(fs::read(&child_draft)?, child_bytes);
    let listed = fixture.run_default(&["list"], "")?;
    success(&listed);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(
        text.contains(parent_body) && !text.contains(child_body),
        "{text}"
    );
    transport.assert_stale_tab(&child)?;

    let returned_child = transport.navigate("sf", 18, "head", "")?;
    assert_eq!(
        fixture.checkout_previous(&returned_child)?,
        transport.revisions[&17].head
    );
    assert_eq!(returned_child, child);
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    assert_eq!(fs::read(&child_draft)?, child_bytes);
    let listed = fixture.run_default(&["list"], "")?;
    success(&listed);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(
        text.contains(child_body) && !text.contains(parent_body),
        "{text}"
    );
    assert_eq!(fs::read(parent.join("manifest.json"))?, old_manifest);
    assert_eq!(snapshot_files(&parent.join("base"))?, old_base);
    assert_eq!(snapshot_files(&parent.join("head"))?, old_head);
    assert_eq!(fixture.sha("unrelated-local-work")?, local_work);
    assert_eq!(fixture.sha("keep-parent-work")?, parent_work);
    assert_eq!(
        fixture.sha("keep-child-head")?,
        transport.revisions[&18].head
    );
    assert_eq!(fixture.previous_bookmarks()?, legacy_bookmarks);
    assert_eq!(transport.fetch_numbers()?, [17, 18]);
    assert_eq!(fixture.zed_calls()?.len(), 4);
    fixture.jj(&["status"])?;
    assert_eq!(
        fixture.sha("@")?,
        transport.revisions[&18].head,
        "Review state stays ignored after the round trip"
    );
    assert!(
        !fixture
            .jj(&["file", "list"])?
            .contains("scratch/zed-pr-review")
    );
    // Recover by the saved receipt SHAs, not by any user or legacy bookmark.
    fixture.jj(&["edit", &parent_work])?;
    assert_eq!(fixture.sha("@")?, parent_work);
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "ONE\nlocal parent edit\nTHREE\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("parent local.txt"))?,
        "Unsnapshotted work while reviewing the parent\n"
    );
    fixture.jj(&["edit", &local_work])?;
    assert_eq!(fixture.sha("@")?, local_work);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("keep local.txt"))?,
        "Unsnapshotted work\n"
    );
    assert_eq!(fixture.previous_bookmarks()?, legacy_bookmarks);
    Ok(())
}

#[test]
fn navigation_transport_round_trip_non_colocated() -> Result<()> {
    navigation_transport_round_trip(false)
}

#[test]
fn navigation_transport_round_trip_colocated() -> Result<()> {
    navigation_transport_round_trip(true)
}

fn navigation_transport_immutable_round_trip(colocate: bool, terminal: bool) -> Result<()> {
    let transport = NavigationTransport::new(colocate, false)?;
    let fixture = &transport.fixture;
    fixture.set_immutable_heads(&[
        &transport.revisions[&17].head,
        &transport.revisions[&18].head,
    ])?;
    let output = fixture.run_default(&["--pr", PR_URL, "open"], "")?;
    success(&output);
    let parent = transport.assert_checkout_kind(17, true)?;
    let local_work = fixture.checkout_previous(&parent)?;
    assert_eq!(fixture.sha("unrelated-local-work")?, local_work);
    assert_return_command(&output, &local_work);
    assert!(fixture.previous_bookmarks()?.is_empty());
    let parent_checkout = fixture.sha("@")?;
    let parent_draft = transport.comment(&parent, 17, "Keep the immutable parent's comment")?;
    let parent_bytes = fs::read(&parent_draft)?;

    let file = (!terminal).then(|| parent.join("head").join(FILE));
    let stack = fixture.execute(navigation_command(fixture, &["stack"], file.as_deref()), "")?;
    success(&stack);
    let text = String::from_utf8_lossy(&stack.stdout);
    assert!(text.contains("@ #17") && text.contains("#18"), "{text}");
    assert_eq!(fixture.sha("@")?, parent_checkout);
    let output = fixture.execute(navigation_command(fixture, &["sf"], file.as_deref()), "")?;
    success(&output);
    let child = transport.assert_checkout_kind(18, true)?;
    let child_checkout = fixture.sha("@")?;
    let previous_parent = fixture.checkout_previous(&child)?;
    assert_eq!(previous_parent, parent_checkout);
    assert_return_command(&output, &previous_parent);
    assert!(fixture.previous_bookmarks()?.is_empty());
    let child_draft = transport.comment(&child, 18, "Keep the immutable child's comment")?;
    let child_bytes = fs::read(&child_draft)?;
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);

    let output = fixture.execute(
        navigation_command(fixture, &["sb"], Some(&child.join("base").join(FILE))),
        "",
    )?;
    success(&output);
    let returned = transport.assert_checkout_kind(17, true)?;
    assert_eq!(returned, parent);
    let previous_child = fixture.checkout_previous(&returned)?;
    assert_eq!(previous_child, child_checkout);
    assert_return_command(&output, &previous_child);
    assert!(fixture.previous_bookmarks()?.is_empty());
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    assert_eq!(fs::read(&child_draft)?, child_bytes);
    assert_eq!(transport.fetch_numbers()?, [17, 18]);
    assert_eq!(fixture.zed_calls()?.len(), 3);
    let checkout = fixture.sha("@")?;
    fixture.jj(&["status"])?;
    assert_eq!(
        fixture.sha("@")?,
        checkout,
        "Review state must stay ignored"
    );
    assert!(fixture.calls()?.iter().all(|call| call["method"] != "POST"));
    assert!(fixture.editors()?.is_empty());
    // jj can abandon these empty working copies; their full receipt SHAs still work.
    for (previous, head) in [
        (&previous_parent, &transport.revisions[&17].head),
        (&previous_child, &transport.revisions[&18].head),
    ] {
        fixture.jj(&["edit", previous])?;
        assert_eq!(&fixture.assert_empty_child(head)?, previous);
    }
    assert!(fixture.previous_bookmarks()?.is_empty());
    Ok(())
}

#[test]
fn navigation_transport_round_trip_between_immutable_pr_heads_in_zed() -> Result<()> {
    navigation_transport_immutable_round_trip(false, false)
}

#[test]
fn navigation_transport_round_trip_between_immutable_pr_heads_including_terminal() -> Result<()> {
    navigation_transport_immutable_round_trip(true, true)
}

#[test]
fn navigation_transport_multiple_children_open_the_offered_pr_number_and_keep_separate_drafts()
-> Result<()> {
    let transport = NavigationTransport::new(false, true)?;
    let fixture = &transport.fixture;
    let parent = transport.open_parent()?;
    let parent_draft = transport.comment(&parent, 17, "Parent comment stays with PR 17")?;
    let parent_bytes = fs::read(&parent_draft)?;
    // Select the second candidate by PR number, not its position. Only the first
    // line is a choice; the following line must not trigger another navigation.
    let second = transport.navigate("sf", 19, "head", "19\n18\n")?;
    assert_eq!(transport.fetch_numbers()?, [17, 19]);
    let second_draft = transport.comment(&second, 19, "Only PR 19 has this comment")?;
    let second_bytes = fs::read(&second_draft)?;
    transport.assert_stale_tab(&parent)?;
    transport.navigate("sb", 17, "base", "")?;
    let first = transport.navigate("sf", 18, "head", "18\n99\n")?;
    assert_ne!(transport.revisions[&18].head, transport.revisions[&19].head);
    let first_draft = transport.comment(&first, 18, "Only PR 18 has this comment")?;
    let first_bytes = fs::read(&first_draft)?;
    assert_ne!(first_draft.parent(), second_draft.parent());
    assert_eq!(fixture.drafts()?.len(), 3);
    transport.navigate("sb", 17, "head", "")?;
    let returned_second = transport.navigate("sf", 19, "base", " 19 \n")?;
    assert_eq!(returned_second, second);
    for (path, bytes) in [
        (&parent_draft, &parent_bytes),
        (&first_draft, &first_bytes),
        (&second_draft, &second_bytes),
    ] {
        assert_eq!(
            &fs::read(path)?,
            bytes,
            "Navigation must not rewrite another PR's draft"
        );
    }
    let listed = fixture.run_default(&["list"], "")?;
    success(&listed);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains("Only PR 19 has this comment"), "{text}");
    assert!(
        !text.contains("Only PR 18 has this comment")
            && !text.contains("Parent comment stays with PR 17"),
        "{text}"
    );
    transport.assert_stale_tab(&first)?;
    assert_eq!(transport.fetch_numbers()?, [17, 19, 18]);
    assert!(fixture.previous_bookmarks()?.is_empty());
    assert_eq!(fixture.zed_calls()?.len(), 6);
    Ok(())
}

#[test]
fn synchronous_prefetch_prepares_adjacent_prs_without_changing_active_review_or_checkout()
-> Result<()> {
    for args in [
        vec!["--pr", PR_URL, "prefetch"],
        vec!["prefetch", "--pr", "17", "--repo", "acme/widgets"],
    ] {
        let transport = NavigationTransport::new(false, true)?;
        let fixture = &transport.fixture;
        let parent = transport.open_parent()?;
        let parent_draft = transport.comment(&parent, 17, "Keep the active review")?;
        let draft_bytes = fs::read(&parent_draft)?;
        let before = NavigationCheckpoint::capture(fixture)?;
        let snapshots = CleanupSession::capture(&parent)?;
        let bookmarks =
            fixture.jj(&["--ignore-working-copy", "bookmark", "list", "--all-remotes"])?;
        let refs = fixture.git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/remotes",
        ])?;
        let launches = fixture.zed_calls()?;
        let receipt_path = parent.parent().unwrap().join("last-checkout.json");
        let receipt = fs::read(&receipt_path)?;
        fs::write(
            fixture.repo.join(FILE),
            "Unsnapshotted work while prefetching\n",
        )?;
        let fetches_before = transport.fetch_numbers()?.len();
        let calls_before = fixture.calls()?.len();
        // Fixture.command disables automatic workers, not this explicit synchronous one.
        success(&fixture.execute(navigation_command(fixture, &args, None), "")?);
        let after = NavigationCheckpoint::capture(fixture)?;
        assert_eq!(after.head, before.head);
        assert_eq!(after.active, before.active);
        assert_eq!(after.drafts, before.drafts);
        assert_eq!(
            fixture.jj(&["--ignore-working-copy", "bookmark", "list", "--all-remotes"])?,
            bookmarks
        );
        assert_eq!(
            fixture.git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/remotes"
            ])?,
            refs
        );
        assert_eq!(fixture.zed_calls()?, launches);
        assert_eq!(fs::read(&receipt_path)?, receipt);
        assert_eq!(
            fs::read_to_string(fixture.repo.join(FILE))?,
            "Unsnapshotted work while prefetching\n"
        );
        snapshots.assert_kept()?;
        assert_eq!(&transport.fetch_numbers()?[fetches_before..], &[18, 19]);
        assert!(
            !fixture
                .git(&[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/zed-pr-review-prefetch"
                ])?
                .is_empty()
        );
        let calls = fixture.calls()?;
        let worker_calls = &calls[calls_before..];
        assert!(
            worker_calls
                .iter()
                .any(|call| call["endpoint"] == PULLS_ENDPOINT)
        );
        assert!(
            worker_calls
                .iter()
                .all(|call| call["method"] != "POST" && call["args"][0] != "pr")
        );
        let mut prepared = BTreeMap::new();
        for entry in fs::read_dir(&fixture.state)? {
            let store = entry?.path();
            let pointer = store.join("prepared.json");
            if !pointer.is_file() {
                continue;
            }
            let pointer = read_json(&pointer)?;
            let directory = PathBuf::from(
                pointer["directory"]
                    .as_str()
                    .context("Missing prepared session path")?,
            );
            let manifest = read_json(&directory.join("manifest.json"))?;
            let number = manifest["pr"]["number"]
                .as_u64()
                .context("Missing prepared PR number")?;
            if number == 17 {
                continue;
            }
            assert!(matches!(number, 18 | 19), "Unexpected prepared PR {number}");
            let revision = &transport.revisions[&number];
            assert_eq!(manifest["pr"]["head_sha"], revision.head);
            assert_eq!(manifest["pr"]["base_sha"], revision.base);
            assert_eq!(manifest["merge_base"], revision.merge_base);
            assert_eq!(manifest["patches_cached"], true);
            assert_eq!(
                snapshot_files(&directory.join("base"))?,
                revision.base_files
            );
            assert_eq!(
                snapshot_files(&directory.join("head"))?,
                revision.head_files
            );
            assert!(!store.join("draft.json").exists());
            assert!(!store.join("last-checkout.json").exists());
            assert!(worker_calls.iter().any(|call| call["endpoint"]
                == format!("repos/acme/widgets/pulls/{number}/files?per_page=100")));
            prepared.insert(number, directory);
        }
        assert_eq!(prepared.keys().copied().collect::<Vec<_>>(), vec![18, 19]);
        assert!(fixture.editors()?.is_empty());

        // Foreground navigation must still validate the PR, but reuse its fetched
        // history and prepared snapshots rather than fetching or preparing again.
        fs::write(
            fixture.repo.join(FILE),
            &transport.revisions[&17].head_files[FILE],
        )?;
        let fetches = transport.fetch_numbers()?;
        let calls_before = fixture.calls()?.len();
        success(&fixture.execute(
            navigation_command(fixture, &["sf"], Some(&parent.join("head").join(FILE))),
            "19\n",
        )?);
        let child = transport.assert_checkout(19)?;
        assert_eq!(child, prepared[&19]);
        assert_eq!(transport.fetch_numbers()?, fetches);
        let calls = fixture.calls()?;
        let navigation = &calls[calls_before..];
        assert!(
            navigation
                .iter()
                .any(|call| call["endpoint"] == "repos/acme/widgets/pulls/19")
        );
        assert!(
            navigation
                .iter()
                .all(|call| !call["endpoint"].as_str().unwrap_or("").contains("/files?"))
        );
        assert_eq!(fs::read(parent_draft)?, draft_bytes);
        snapshots.assert_kept()?;
        let calls = fixture.calls()?;
        transport.comment(&child, 19, "Comment on the prefetched child")?;
        assert_eq!(fixture.calls()?, calls);
        assert_eq!(fixture.drafts()?.len(), 2);
        assert!(fixture.posts()?.is_empty());
    }
    Ok(())
}

fn wait_for_prefetch(fixture: &Fixture) -> Result<Value> {
    let started = Instant::now();
    loop {
        let status = fixture.state.join("prefetch/status.json");
        if status.is_file() {
            let status = read_json(&status)?;
            if matches!(status["state"].as_str(), Some("complete" | "failed")) {
                let lock = File::open(fixture.state.join("prefetch/lock"))?;
                if lock.try_lock().is_ok() {
                    return Ok(status);
                }
            }
        }
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "Prefetch did not finish within 30 seconds"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn automatic_prefetch_starts_after_stack_and_leaves_the_active_review_unchanged() -> Result<()> {
    let transport = NavigationTransport::new(false, true)?;
    let fixture = &transport.fixture;
    let parent = transport.open_parent()?;
    transport.comment(&parent, 17, "Keep the active review")?;
    let before = NavigationCheckpoint::capture(fixture)?;
    let snapshots = CleanupSession::capture(&parent)?;
    let bookmarks = fixture.jj(&["--ignore-working-copy", "bookmark", "list", "--all-remotes"])?;
    let refs = fixture.git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/heads",
        "refs/remotes",
    ])?;
    let launches = fixture.zed_calls()?;
    fs::write(
        fixture.repo.join(FILE),
        "Unsnapshotted work during automatic preparation\n",
    )?;
    assert!(!fixture.state.join("prefetch/status.json").exists());
    let mut command =
        navigation_command(fixture, &["stack"], Some(&parent.join("head").join(FILE)));
    command.env_remove("ZED_PR_REVIEW_PREFETCH");
    success(&bounded_output(command, "", &fixture.root)?);
    // Check completion only after the foreground task has returned. The worker
    // must own neither its terminal streams nor the active review context.
    let status = wait_for_prefetch(fixture)?;
    assert_eq!(status["state"], "complete", "{status}");
    assert_eq!(status["ready"], 2);
    assert_eq!(status["total"], 2);
    assert_eq!(status["errors"], json!([]));
    let after = NavigationCheckpoint::capture(fixture)?;
    assert_eq!(after.head, before.head);
    assert_eq!(after.active, before.active);
    assert_eq!(after.drafts, before.drafts);
    snapshots.assert_kept()?;
    assert_eq!(
        fixture.jj(&["--ignore-working-copy", "bookmark", "list", "--all-remotes"])?,
        bookmarks
    );
    assert_eq!(
        fixture.git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/remotes"
        ])?,
        refs
    );
    assert_eq!(fixture.zed_calls()?, launches);
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "Unsnapshotted work during automatic preparation\n"
    );
    assert_eq!(transport.fetch_numbers()?, vec![17, 18, 19]);
    for number in [18, 19] {
        let prepared = transport
            .prepared_directory(number)?
            .context("Automatic worker did not prepare adjacent PR")?;
        assert_eq!(
            snapshot_files(&prepared.join("head"))?,
            transport.revisions[&number].head_files
        );
    }
    assert!(
        fixture
            .calls()?
            .iter()
            .all(|call| call["method"] != "POST" && call.get("unexpected").is_none())
    );
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn prefetch_serializes_workers_without_blocking_comments_on_the_active_pr() -> Result<()> {
    let transport = NavigationTransport::new(false, false)?;
    let fixture = &transport.fixture;
    let parent = transport.open_parent()?;
    let before = NavigationCheckpoint::capture(fixture)?;
    support::install(
        &fixture.root.join("bin/gh"),
        &json!({"tool": "gate", "root": fixture.root}),
    )?;
    let command = navigation_command(fixture, &["--pr", PR_URL, "prefetch"], None);
    let root = fixture.root.clone();
    let worker = thread::spawn(move || bounded_output(command, "", &root));
    let contended = (|| -> Result<()> {
        let started = Instant::now();
        while !fixture.root.join("prefetch-entered").exists() {
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "Worker never reached session preparation"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let status = read_json(&fixture.state.join("prefetch/status.json"))?;
        assert_eq!(status["state"], "fetching");
        assert_eq!(status["pr"], 18);
        // Slow network reads must not retain even the target PR's draft lock.
        let target_store = fixture.state.join(status["draft_key"].as_str().unwrap());
        drop(Store::at(&target_store)?);
        let fetches = transport.fetch_numbers()?;
        success(&fixture.execute(
            navigation_command(fixture, &["--pr", PR_URL, "prefetch"], None),
            "",
        )?);
        assert_eq!(
            read_json(&fixture.state.join("prefetch/status.json"))?,
            status
        );
        assert_eq!(transport.fetch_numbers()?, fetches);
        let calls = fixture.calls()?;
        transport.comment(
            &parent,
            17,
            "Foreground comment while another PR is preparing",
        )?;
        assert_eq!(fixture.calls()?, calls);
        assert_eq!(fixture.sha("@")?, before.head);
        assert_eq!(fs::read(fixture.state.join("active.json"))?, before.active);
        Ok(())
    })();
    // Release and reap even when a contention assertion returns an error.
    fs::write(fixture.root.join("prefetch-release"), "")?;
    let output = worker.join().expect("Prefetch test thread panicked")?;
    contended?;
    success(&output);
    let status = wait_for_prefetch(fixture)?;
    assert_eq!(status["state"], "complete");
    assert_eq!(status["ready"], 1);
    assert_eq!(status["errors"], json!([]));
    assert_eq!(transport.fetch_numbers()?, vec![17, 18]);
    let prepared = transport
        .prepared_directory(18)?
        .context("Missing child session")?;
    assert!(!prepared.parent().unwrap().join("draft.json").exists());
    assert_eq!(fixture.drafts()?.len(), 1);
    assert_eq!(fixture.draft()?["comments"].as_array().unwrap().len(), 1);
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

fn cleanup_contents(directory: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            for (path, bytes) in cleanup_contents(&entry.path())? {
                files.insert(PathBuf::from(entry.file_name()).join(path), bytes);
            }
        } else {
            files.insert(PathBuf::from(entry.file_name()), fs::read(entry.path())?);
        }
    }
    Ok(files)
}

struct CleanupSession {
    directory: PathBuf,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl CleanupSession {
    fn capture(directory: &Path) -> Result<Self> {
        assert!(directory.join("base").is_dir());
        assert!(directory.join("head").is_dir());
        assert!(!directory.join("cleanup.json").exists());
        Ok(Self {
            directory: directory.to_owned(),
            files: cleanup_contents(directory)?,
        })
    }

    fn assert_kept(&self) -> Result<()> {
        assert!(self.directory.join("base").is_dir());
        assert!(self.directory.join("head").is_dir());
        assert_eq!(cleanup_contents(&self.directory)?, self.files);
        Ok(())
    }

    fn assert_cleaned(&self, receipt: &Value) -> Result<()> {
        use sha2::{Digest, Sha256};
        let files = cleanup_contents(&self.directory)?;
        assert!(!self.directory.join("base").exists());
        assert!(!self.directory.join("head").exists());
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            [
                PathBuf::from("cleanup.json"),
                PathBuf::from("manifest.json")
            ]
        );
        let manifest = &self.files[Path::new("manifest.json")];
        assert_eq!(&files[Path::new("manifest.json")], manifest);
        let marker = &files[Path::new("cleanup.json")];
        assert!(marker.len() < 1024, "Keep only a small cleanup marker");
        assert_eq!(
            serde_json::from_slice::<Value>(marker)?,
            json!({
                "version": 1,
                "manifest_sha256": format!("{:x}", Sha256::digest(manifest)),
                "review_id": receipt["id"],
                "review_state": receipt["state"],
                "review_url": receipt["url"],
            })
        );
        Ok(())
    }
}

fn cleanup_assert_receipt(path: &Path, before: &Value, state: &str) -> Result<Value> {
    let saved = read_json(path)?;
    let receipt = json!({
        "id": 501, "state": state,
        "url": format!("{}#pullrequestreview-501", before["pr"]["url"].as_str().unwrap()),
    });
    let mut expected = before.clone();
    expected["attempt"] = Value::Null;
    expected["submitted"] = receipt.clone();
    assert_eq!(
        saved, expected,
        "Keep comments, revision, and retry history"
    );
    Ok(receipt)
}

#[test]
fn cleanup_all_opens_and_old_revisions_keep_other_reviewer_and_draft_state() -> Result<()> {
    let fixture = navigation_fixture()?;
    let old = CleanupSession::capture(&fixture.prepare_session()?)?;
    fixture.jj(&["new", "-m", "A newer revision of the same PR"])?;
    let head = fixture.sha("@")?;
    fixture.configure("head", json!(head))?;
    fixture.configure("reviewer", json!("another-reviewer"))?;
    let other = CleanupSession::capture(&fixture.prepare_session()?)?;
    success(&fixture.run_session(
        &other.directory,
        &["comment"],
        "Other reviewer's private comment",
    )?);
    let other_draft = other.directory.parent().unwrap().join("draft.json");
    let other_bytes = fs::read(&other_draft)?;
    fixture.configure("reviewer", json!("reviewer"))?;
    let first = CleanupSession::capture(&fixture.prepare_session()?)?;
    let latest = CleanupSession::capture(&fixture.prepare_session()?)?;
    assert_ne!(old.directory, first.directory);
    assert_ne!(first.directory, latest.directory);
    success(&fixture.run_session(
        &latest.directory,
        &["comment"],
        "Keep my original inline comment",
    )?);
    let draft_path = latest.directory.parent().unwrap().join("draft.json");
    let before = read_json(&draft_path)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    fixture.configure(
        "observe_sessions",
        json!([old.directory, first.directory, latest.directory]),
    )?;
    success(&fixture.run_session(&latest.directory, &["approve"], "")?);
    let receipt = cleanup_assert_receipt(&draft_path, &before, "APPROVED")?;
    for session in [&old, &first, &latest] {
        session.assert_cleaned(&receipt)?;
    }
    other.assert_kept()?;
    assert_eq!(fs::read(other_draft)?, other_bytes);
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    let posts = fixture.posts()?;
    assert_eq!(posts.len(), 1);
    for session in posts[0]["sessions_at_post"].as_object().unwrap().values() {
        assert_eq!(
            session,
            &json!({"base": true, "head": true, "cleanup.json": false})
        );
    }
    assert!(
        posts[0]["drafts_at_post"]
            .as_array()
            .unwrap()
            .iter()
            .any(|draft| {
                draft["pr"]["reviewer"] == "reviewer"
                    && !draft["attempt"].is_null()
                    && draft["submitted"].is_null()
            })
    );
    assert_eq!(fixture.sha("@")?, head);
    Ok(())
}

#[test]
fn cleanup_blank_canceled_and_pending_reviews_preserve_snapshots_and_draft() -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let session = CleanupSession::capture(&directory)?;
    let draft_path = fixture.draft_path()?;
    let before = fs::read(&draft_path)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    for mode in ["write", "unchanged", "fail"] {
        fixture.configure("editor_mode", json!(mode))?;
        fixture.configure("editor_body", json!(" \n\t"))?;
        for action in ["approve", "request-changes"] {
            let output = fixture.run_session(&directory, &[action], "")?;
            if mode == "fail" {
                failure(&output);
            } else {
                success(&output);
            }
            session.assert_kept()?;
            assert_eq!(fs::read(&draft_path)?, before);
        }
    }
    let pending = json!([{"id": 70, "state": "PENDING", "user": {"login": "reviewer"}}]);
    fixture.configure("reviews", pending.clone())?;
    let editors = fixture.editors()?.len();
    for action in ["approve", "request-changes"] {
        failure(&fixture.run_session(&directory, &[action], "")?);
        session.assert_kept()?;
        assert_eq!(fs::read(&draft_path)?, before);
        assert_eq!(fixture.control()?["reviews"], pending);
    }
    assert_eq!(fixture.editors()?.len(), editors);
    assert!(fixture.posts()?.is_empty());
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    Ok(())
}

#[test]
fn cleanup_failed_and_uncertain_submissions_preserve_snapshots_and_attempts() -> Result<()> {
    for (mode, action) in [
        ("fail", "approve"),
        ("lost_response", "request-changes"),
        ("delayed_acceptance", "approve"),
        ("invalid_receipt", "request-changes"),
    ] {
        let (fixture, directory) = navigation_session()?;
        let session = CleanupSession::capture(&directory)?;
        let comments = fixture.draft()?["comments"].clone();
        let active = fs::read(fixture.state.join("active.json"))?;
        fixture.configure("post", json!(mode))?;
        failure(&fixture.run_session(&directory, &[action], "")?);
        let uncertain = fixture.draft()?;
        let bytes = fs::read(fixture.draft_path()?)?;
        assert_eq!(uncertain["comments"], comments);
        assert!(uncertain["submitted"].is_null());
        assert_eq!(
            uncertain["attempt"]["payload"],
            fixture.posts()?[0]["payload"]
        );
        for action in ["approve", "request-changes"] {
            failure(&fixture.run_session(&directory, &[action], "")?);
            assert_eq!(fs::read(fixture.draft_path()?)?, bytes);
        }
        if matches!(mode, "fail" | "delayed_acceptance") {
            success(&fixture.run_default(&["recover"], "")?);
            success(&fixture.saved(&["recover", "--retry"], "no\n")?);
            assert_eq!(fs::read(fixture.draft_path()?)?, bytes);
        }
        session.assert_kept()?;
        assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.editors()?.len(), 1);
    }
    Ok(())
}

fn cleanup_recovery(saved: bool) -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let old = CleanupSession::capture(&directory)?;
    let latest = CleanupSession::capture(&fixture.prepare_session()?)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    fixture.configure("post", json!("lost_response"))?;
    let (action, state) = if saved {
        ("request-changes", "CHANGES_REQUESTED")
    } else {
        ("approve", "APPROVED")
    };
    failure(&fixture.run_session(&latest.directory, &[action], "")?);
    let uncertain = fixture.draft()?;
    old.assert_kept()?;
    latest.assert_kept()?;
    let calls_before = fixture.calls()?.len();
    let output = if saved {
        fixture.jj(&["new", "-m", "Recover after leaving the reviewed head"])?;
        fixture.configure("deny_discovery", json!(true))?;
        // No repository is available from cwd or Zed's visible-root fallback.
        let mut command = fixture.command(env!("CARGO_BIN_EXE_zed-pr-review"));
        command
            .current_dir(&fixture.root)
            .env("ZED_WORKTREE_ROOT", &fixture.root)
            .arg("--draft-dir")
            .arg(directory.parent().unwrap())
            .arg("recover");
        fixture.execute(command, "")?
    } else {
        fixture.execute(
            navigation_command(
                &fixture,
                &["recover"],
                Some(&directory.join("base").join(FILE)),
            ),
            "",
        )?
    };
    success(&output);
    let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &uncertain, state)?;
    old.assert_cleaned(&receipt)?;
    latest.assert_cleaned(&receipt)?;
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    if saved {
        assert!(fixture.calls()?[calls_before..].iter().all(|call| {
            call["method"] == "GET"
                && (call["endpoint"] == "user"
                    || call["endpoint"].as_str().unwrap_or("").contains("/reviews"))
        }));
    }
    Ok(())
}

#[test]
fn cleanup_recover_confirmed_receipt_without_reposting() -> Result<()> {
    cleanup_recovery(false)
}

#[test]
fn cleanup_recover_via_draft_dir_outside_repository_without_reposting() -> Result<()> {
    cleanup_recovery(true)
}

#[test]
fn cleanup_delayed_receipt_during_retry_preserves_history_without_reposting() -> Result<()> {
    for during_editor in [false, true] {
        let (fixture, directory) = navigation_session()?;
        let session = CleanupSession::capture(&directory)?;
        fixture.configure("post", json!("delayed_acceptance"))?;
        failure(&fixture.run_session(&directory, &["approve"], "")?);
        success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
        let before = fixture.draft()?;
        assert_eq!(before["retry_history"].as_array().unwrap().len(), 1);
        let archived = archives(directory.parent().unwrap())?;
        assert_eq!(archived.len(), 1);

        session.assert_kept()?;
        if during_editor {
            fixture.configure("editor_mode", json!("accept_delayed_review"))?;
        } else {
            fixture.accept_delayed_review()?;
        }
        fixture.configure("post", json!("success"))?;
        success(&fixture.run_session(&directory, &["request-changes"], "")?);
        let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &before, "APPROVED")?;
        session.assert_cleaned(&receipt)?;
        assert_eq!(archives(directory.parent().unwrap())?, archived);
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.editors()?.len(), if during_editor { 2 } else { 1 });
    }
    Ok(())
}

#[test]
fn cleanup_failure_warns_but_receipt_blocks_duplicates_and_recover_retries() -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let old = CleanupSession::capture(&directory)?;
    let latest = CleanupSession::capture(&fixture.prepare_session()?)?;
    let selected = latest.directory.join("head").join(FILE);
    let snapshot = fs::read(&selected)?;
    let before = fixture.draft()?;
    let active = fs::read(fixture.state.join("active.json"))?;
    fixture.configure("editor_mode", json!("alter_snapshot"))?;
    fixture.configure("editor_snapshot", json!(selected))?;
    let mut command = fixture.default_cli(&["approve"]);
    command.env("ZED_FILE", &selected);
    let output = fixture.execute(command, "")?;
    success(&output);
    let warning = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    assert!(
        warning.contains("warn") && warning.contains("cleanup"),
        "{warning}"
    );
    let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &before, "APPROVED")?;
    let submitted = fs::read(fixture.draft_path()?)?;
    old.assert_kept()?;
    assert!(latest.directory.join("head").is_dir());
    assert!(latest.directory.join("base").is_dir());
    assert!(!latest.directory.join("cleanup.json").exists());
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    // Repair the fixture's corruption, then retry only cleanup, not the review.
    fs::write(&selected, snapshot)?;
    for action in ["approve", "request-changes"] {
        let output = fixture.run_session(&latest.directory, &[action], "")?;
        failure(&output);
        assert!(describe(&output).contains("already been submitted"));
    }
    success(&fixture.saved(&["recover"], "")?);
    old.assert_cleaned(&receipt)?;
    latest.assert_cleaned(&receipt)?;
    assert_eq!(fs::read(fixture.draft_path()?)?, submitted);
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    Ok(())
}

#[test]
fn cleanup_receipt_save_failure_keeps_snapshots_and_pending_attempt_until_recovery() -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let session = CleanupSession::capture(&directory)?;
    let comments = fixture.draft()?["comments"].clone();
    let active = fs::read(fixture.state.join("active.json"))?;
    fixture.configure("post", json!("receipt_save_failure"))?;
    let output = fixture.run_session(&directory, &["request-changes"], "")?;
    failure(&output);
    assert!(describe(&output).contains("saving the receipt failed"));
    session.assert_kept()?;
    let uncertain = fixture.draft()?;
    assert!(uncertain["submitted"].is_null());
    assert_eq!(uncertain["comments"], comments);
    assert_eq!(uncertain, fixture.posts()?[0]["drafts_at_post"][0]);
    assert!(!uncertain["attempt"].is_null());
    assert_eq!(fixture.control()?["reviews"].as_array().unwrap().len(), 1);
    fs::remove_file(directory.parent().unwrap().join("receipt-save-blocker"))?;
    failure(&fixture.run_session(&directory, &["approve"], "")?);
    assert_eq!(fixture.draft()?, uncertain);
    success(&fixture.saved(&["recover"], "")?);
    let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &uncertain, "CHANGES_REQUESTED")?;
    session.assert_cleaned(&receipt)?;
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    Ok(())
}

// Omit --directory so removed snapshot ancestors, not Zed's visible root or
// the task's cwd, determine the workspace in a multi-root window.
fn cleanup_context_command(
    fixture: &Fixture,
    args: &[&str],
    file: Option<&Path>,
    fallback: &Path,
) -> Command {
    let mut command = fixture.command(env!("CARGO_BIN_EXE_zed-pr-review"));
    command
        .current_dir(fallback)
        .args(args)
        .env("ZED_WORKTREE_ROOT", fallback);
    if let Some(file) = file {
        command.env("ZED_FILE", file);
    }
    command
}

fn cleanup_navigate(
    transport: &NavigationTransport,
    command: Command,
    number: u64,
) -> Result<PathBuf> {
    let fixture = &transport.fixture;
    let before = NavigationCheckpoint::capture(fixture)?;
    let fetches = transport.fetch_numbers()?;
    let launches = fixture.zed_calls()?.len();
    let bookmarks = fixture.previous_bookmarks()?;
    let posts: Vec<_> = fixture
        .calls()?
        .into_iter()
        .filter(|call| call["method"] == "POST")
        .collect();
    let output = fixture.execute(command, "")?;
    success(&output);
    let session = transport.cleanup_assert_checkout(number)?;
    let previous = fixture.checkout_previous(&session)?;
    assert_eq!(previous, before.head);
    assert_return_command(&output, &previous);
    let expected_fetches = if fetches.contains(&number) {
        vec![]
    } else {
        vec![number]
    };
    assert_eq!(
        &transport.fetch_numbers()?[fetches.len()..],
        expected_fetches
    );
    assert_eq!(fixture.zed_calls()?.len(), launches + 1);
    assert_eq!(fixture.previous_bookmarks()?, bookmarks);
    assert_eq!(
        NavigationCheckpoint::capture(fixture)?.drafts,
        before.drafts
    );
    assert_eq!(
        fixture
            .calls()?
            .into_iter()
            .filter(|call| call["method"] == "POST")
            .collect::<Vec<_>>(),
        posts
    );
    Ok(session)
}

fn cleanup_navigation_round_trip(action: &str, state: &str, colocate: bool) -> Result<()> {
    let transport = NavigationTransport::new(colocate, false)?;
    let fixture = &transport.fixture;
    let first_parent = transport.open_parent()?;
    let parent_draft = transport.comment(&first_parent, 17, "Keep the parent's review comment")?;
    let first_child = transport.navigate("sf", 18, "base", "")?;
    let child_draft = transport.comment(&first_child, 18, "Keep the child's separate comment")?;
    let parent = transport.navigate("sb", 17, "head", "")?;
    let old_parent = CleanupSession::capture(&first_parent)?;
    let parent_session = CleanupSession::capture(&parent)?;
    let child_session = CleanupSession::capture(&first_child)?;
    let parent_before = read_json(&parent_draft)?;
    let child_before = read_json(&child_draft)?;
    let child_bytes = fs::read(&child_draft)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    let checkout_receipt_path = parent.parent().unwrap().join("last-checkout.json");
    let checkout_receipt = fs::read(&checkout_receipt_path)?;
    let bookmarks = fixture.previous_bookmarks()?;
    assert!(bookmarks.is_empty());
    let wrong_root = Fixture::new()?;
    let wrong_head = wrong_root.sha("@")?;
    let selected = parent
        .join(if action == "approve" { "head" } else { "base" })
        .join(FILE);
    success(&fixture.execute(
        cleanup_context_command(fixture, &[action], Some(&selected), &wrong_root.repo),
        "",
    )?);
    let parent_receipt = cleanup_assert_receipt(&parent_draft, &parent_before, state)?;
    old_parent.assert_cleaned(&parent_receipt)?;
    parent_session.assert_cleaned(&parent_receipt)?;
    child_session.assert_kept()?;
    assert_eq!(fs::read(&checkout_receipt_path)?, checkout_receipt);
    assert_eq!(fixture.previous_bookmarks()?, bookmarks);
    assert_eq!(fs::read(&child_draft)?, child_bytes);
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    let parent_bytes = fs::read(&parent_draft)?;
    for file in [
        Some(parent.join("head").join(FILE)),
        Some(parent.join("base").join(FILE)),
        None,
    ] {
        let fallback = if file.is_some() {
            &wrong_root.repo
        } else {
            &fixture.repo
        };
        let output = fixture.execute(
            cleanup_context_command(fixture, &["stack"], file.as_deref(), fallback),
            "",
        )?;
        success(&output);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("@ #17") && text.contains("#18"), "{text}");
    }
    for side in ["head", "base"] {
        let file = parent.join(side).join(FILE);
        for action in ["list", "recover"] {
            let output = fixture.execute(
                cleanup_context_command(fixture, &[action], Some(&file), &wrong_root.repo),
                "",
            )?;
            success(&output);
            if action == "list" {
                assert!(describe(&output).contains("Keep the parent's review comment"));
            }
        }
        for action in ["approve", "request-changes"] {
            let output = fixture.execute(
                cleanup_context_command(fixture, &[action], Some(&file), &wrong_root.repo),
                "",
            )?;
            failure(&output);
        }
    }
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    let child = cleanup_navigate(
        &transport,
        cleanup_context_command(fixture, &["sf"], Some(&selected), &wrong_root.repo),
        18,
    )?;
    let second_child = CleanupSession::capture(&child)?;
    // A marker preserves the old PR identity; it must not select the new active PR.
    let checkpoint = NavigationCheckpoint::capture(fixture)?;
    let fetches = transport.fetch_numbers()?;
    for action in ["stack", "sb", "sf"] {
        let output = fixture.execute(
            cleanup_context_command(fixture, &[action], Some(&selected), &wrong_root.repo),
            "",
        )?;
        navigation_failure(&output);
        let text = describe(&output).to_ascii_lowercase();
        assert!(text.contains("head") || text.contains("stale"), "{text}");
    }
    assert_eq!(
        NavigationCheckpoint::capture(fixture)?.drafts,
        checkpoint.drafts
    );
    assert_eq!(
        fs::read(fixture.state.join("active.json"))?,
        checkpoint.active
    );
    assert_eq!(fixture.sha("@")?, checkpoint.head);
    assert_eq!(transport.fetch_numbers()?, fetches);
    success(&fixture.run_session(&child, &[action], "")?);
    let child_receipt = cleanup_assert_receipt(&child_draft, &child_before, state)?;
    child_session.assert_cleaned(&child_receipt)?;
    second_child.assert_cleaned(&child_receipt)?;
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    let child_bytes = fs::read(&child_draft)?;
    let child_file = child.join("base").join(FILE);
    let (file, fallback) = if action == "approve" {
        (None, &fixture.repo)
    } else {
        (Some(child_file.as_path()), &wrong_root.repo)
    };
    let returned_parent = cleanup_navigate(
        &transport,
        cleanup_context_command(fixture, &["sb"], file, fallback),
        17,
    )?;
    assert_ne!(returned_parent, parent);
    let reopened = CleanupSession::capture(&returned_parent)?;
    success(&fixture.execute(
        cleanup_context_command(
            fixture,
            &["recover"],
            Some(&returned_parent.join("head").join(FILE)),
            &wrong_root.repo,
        ),
        "",
    )?);
    reopened.assert_cleaned(&parent_receipt)?;
    let returned_child = cleanup_navigate(
        &transport,
        cleanup_context_command(fixture, &["sf"], None, &fixture.repo),
        18,
    )?;
    assert_ne!(returned_child, child);
    assert_eq!(fs::read(&parent_draft)?, parent_bytes);
    assert_eq!(fs::read(&child_draft)?, child_bytes);
    assert_eq!(transport.fetch_numbers()?, [17, 18]);
    assert_eq!(fixture.previous_bookmarks()?, bookmarks);
    assert_eq!(fixture.zed_calls()?.len(), 6);
    assert_eq!(
        fixture
            .calls()?
            .iter()
            .filter(|call| call["method"] == "POST")
            .count(),
        2
    );
    assert_eq!(fixture.editors()?.len(), 2);
    assert_eq!(wrong_root.sha("@")?, wrong_head);
    assert!(!wrong_root.repo.join("scratch/zed-pr-review").exists());
    fixture.jj(&["status"])?;
    assert_eq!(fixture.sha("@")?, transport.revisions[&18].head);
    assert!(
        !fixture
            .jj(&["file", "list"])?
            .contains("scratch/zed-pr-review")
    );
    Ok(())
}

#[test]
fn cleanup_approve_keeps_full_navigation_from_removed_paths_and_active_terminal() -> Result<()> {
    cleanup_navigation_round_trip("approve", "APPROVED", false)
}

#[test]
fn cleanup_request_changes_keeps_full_navigation_from_removed_paths_and_active_terminal()
-> Result<()> {
    cleanup_navigation_round_trip("request-changes", "CHANGES_REQUESTED", true)
}

#[test]
fn cleanup_removed_snapshots_reject_comments_unknown_paths_and_revision_drift() -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let session = CleanupSession::capture(&directory)?;
    success(&fixture.run_session(&directory, &["approve"], "")?);
    session.assert_cleaned(&fixture.draft()?["submitted"])?;
    let files = cleanup_contents(&directory)?;
    let draft = fs::read(fixture.draft_path()?)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    let mut command = fixture.default_cli(&["comment"]);
    command.env("ZED_FILE", directory.join("head").join(FILE));
    let output = fixture.execute(command, "Cannot comment on removed copies")?;
    failure(&output);
    assert!(describe(&output).to_ascii_lowercase().contains("clean"));
    for path in [
        directory.join("head/not-registered.txt"),
        directory.join("base/not-registered.txt"),
    ] {
        navigation_failure(
            &fixture.execute(navigation_command(&fixture, &["stack"], Some(&path)), "")?,
        );
    }
    let original = fixture.control()?;
    for (field, value) in [
        ("head", fixture.base.as_str()),
        ("base", fixture.head.as_str()),
        ("head_ref", "different-branch"),
        ("head_repo", "another/widgets"),
        ("reviewer", "another-reviewer"),
    ] {
        fixture.configure(field, json!(value))?;
        for action in ["stack", "sf", "list"] {
            let output = fixture.execute(
                navigation_command(
                    &fixture,
                    &[action],
                    Some(&directory.join("base").join(FILE)),
                ),
                "",
            )?;
            navigation_failure(&output);
        }
        fixture.set_control(original.clone())?;
    }
    assert_eq!(cleanup_contents(&directory)?, files);
    assert_eq!(fs::read(fixture.draft_path()?)?, draft);
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    assert!(fixture.zed_calls()?.is_empty());
    assert_eq!(fixture.sha("@")?, fixture.head);
    Ok(())
}

#[test]
fn open_blank_input_cancels_before_any_repository_discovery() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure("deny_discovery", json!(true))?;
    for input in ["", " \t\n", "\n17\n"] {
        let mut command = fixture.command(env!("CARGO_BIN_EXE_zed-pr-review"));
        // This is not a jj workspace. Cancellation must happen before resolving it.
        command
            .arg("--directory")
            .arg(fixture.root.join("home"))
            .arg("open");
        let output = fixture.execute(command, input)?;
        success(&output);
        assert!(describe(&output).to_ascii_lowercase().contains("cancel"));
    }
    assert!(fixture.calls()?.is_empty());
    assert!(fixture.zed_calls()?.is_empty());
    assert!(!fixture.state.exists());
    assert!(!fixture.repo.join("scratch").exists());
    assert_eq!(fixture.sha("@")?, fixture.head);
    Ok(())
}

#[test]
fn open_fetches_the_exact_pr_and_full_history_preserves_work_and_launches_from_root() -> Result<()>
{
    for colocate in [false, true] {
        let transport = OpenFixture::new(colocate)?;
        let fixture = &transport.fixture;
        let old_head = fixture.sha("@")?;
        let mut command = fixture.default_cli(&["--pr", "17", "open"]);
        command.current_dir(fixture.root.join("home"));
        // An explicit global PR wins even when stdin supplies an invalid value.
        let output = fixture.execute(command, "not a PR\n")?;
        success(&output);
        assert_eq!(fixture.sha("@")?, fixture.head);
        assert_ne!(fixture.head, old_head);
        assert_eq!(
            fs::read_to_string(fixture.repo.join(FILE))?,
            "ONE\ntwo\nTHREE\n"
        );
        assert!(!fixture.repo.join("keep local.txt").exists());
        assert_eq!(
            fixture.git(&["merge-base", &fixture.head, &fixture.base])?,
            transport.common
        );
        assert_eq!(
            fixture.sha("@-")?,
            transport.first,
            "Edit the exact head, not a new child"
        );
        let session = fixture.active_directory()?;
        assert!(session.starts_with(&fixture.state));
        let manifest = read_json(&session.join("manifest.json"))?;
        assert_eq!(manifest["root"], json!(fixture.repo));
        assert_eq!(manifest["directory"], json!(session));
        assert_eq!(manifest["pr"]["head_sha"], fixture.head);
        assert_eq!(manifest["pr"]["base_sha"], fixture.base);
        assert_eq!(manifest["pr"]["head_repo"], "contributor/widgets");
        assert_eq!(manifest["merge_base"], transport.common);
        assert_ne!(transport.common, fixture.base);
        assert_ne!(transport.common, transport.first);
        assert_eq!(manifest["files"].as_array().unwrap().len(), 4);
        assert_eq!(
            fs::read_to_string(session.join("base").join(FILE))?,
            "one\ntwo\nthree\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("head").join(FILE))?,
            "ONE\ntwo\nTHREE\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("head/early change.txt"))?,
            "first commit\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("head/late change.txt"))?,
            "second commit\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("base/removed.txt"))?,
            "old\n"
        );
        assert!(!session.join("head/removed.txt").exists());
        for side in ["base", "head"] {
            assert!(!session.join(side).join("unchanged.txt").exists());
            assert!(!session.join(side).join("base-only.txt").exists());
        }
        assert!(
            !fixture
                .git(&["diff", "--name-only", &transport.first, &fixture.head])?
                .lines()
                .any(|path| path == "early change.txt")
        );
        let bookmarks = fixture.jj(&[
            "--ignore-working-copy",
            "bookmark",
            "list",
            "-T",
            "name ++ \"\\n\"",
        ])?;
        assert!(fixture.previous_bookmarks()?.is_empty(), "{bookmarks}");
        let previous = fixture.checkout_previous(&session)?;
        assert_eq!(fixture.sha("unrelated-local-work")?, previous);
        assert_return_command(&output, &previous);
        assert_ne!(
            previous, old_head,
            "Preserve unsnapshotted edits, not just the old recorded commit"
        );
        assert_eq!(
            fixture.git(&["show", &format!("{previous}:keep local.txt")])?,
            "Unsnapshotted work"
        );
        assert_eq!(
            fixture.git(&["show", &format!("{previous}:{FILE}")])?,
            "one\ntwo\nthree"
        );
        let prior_files = fixture.git(&["ls-tree", "-r", "--name-only", &previous])?;
        assert!(
            !prior_files.contains("scratch/zed-pr-review"),
            "Do not preserve private state in the previous commit"
        );
        let launches = fixture.zed_calls()?;
        assert_eq!(launches.len(), 1);
        let launch = &launches[0];
        assert_eq!(launch["cwd"], json!(fixture.repo));
        assert_eq!(
            launch["args"],
            json!([
                "--existing",
                "--diff",
                session.join("base"),
                session.join("head")
            ])
        );
        assert_eq!(
            launch["active"],
            read_json(&fixture.state.join("active.json"))?,
            "Activate before launching Zed"
        );
        let receipts = launch["checkouts"]
            .as_object()
            .context("Missing checkout receipts")?;
        assert_eq!(
            receipts.len(),
            1,
            "Write last-checkout.json before launching Zed"
        );
        let (path, receipt) = receipts.iter().next().unwrap();
        assert_eq!(*receipt, read_json(&fixture.state.join(path))?);
        assert_eq!(receipt["previous_commit"], previous);
        assert_eq!(receipt["pr"]["head_sha"], fixture.head);
        let calls = read_json_lines(&fixture.root.join("git.jsonl"))?;
        let fetches: Vec<_> = calls
            .iter()
            .filter(|call| {
                call["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "fetch")
            })
            .collect();
        assert_eq!(
            fetches.len(),
            1,
            "Fetch the PR ref and base history together"
        );
        let gh_calls = fixture.calls()?;
        let discovery = gh_calls
            .iter()
            .find(|call| call["args"][0] == "repo")
            .context("Numeric open must discover the current GitHub repository")?;
        assert_eq!(discovery["cwd"], json!(fixture.repo));
        assert_eq!(
            discovery["git_dir"],
            fixture.jj(&["--ignore-working-copy", "git", "root"])?
        );
        assert!(fixture.calls()?.iter().all(|call| call["args"][0] != "pr"));
        assert!(fixture.calls()?.iter().all(|call| call["method"] != "POST"));
        assert!(fixture.editors()?.is_empty());
        let mut comment = fixture.default_cli(&["comment"]);
        comment
            .env("ZED_FILE", session.join("head/early change.txt"))
            .env("ZED_SELECTED_TEXT", "first commit\n");
        success(&fixture.execute(comment, "Review a change from the first PR commit")?);
        assert_eq!(fixture.draft()?["comments"][0]["path"], "early change.txt");
        assert_eq!(fixture.draft()?["pr"]["head_sha"], fixture.head);
        assert_eq!(fixture.draft()?["pr"]["base_sha"], fixture.base);
        assert!(fixture.posts()?.is_empty());
        fixture.assert_default_state_ignored()?;
        fixture.jj(&["edit", &previous])?;
        assert_eq!(fixture.sha("@")?, previous);
        assert_eq!(
            fs::read_to_string(fixture.repo.join("keep local.txt"))?,
            "Unsnapshotted work\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.repo.join(FILE))?,
            "one\ntwo\nthree\n"
        );
        assert!(fixture.previous_bookmarks()?.is_empty());
    }
    Ok(())
}

#[test]
fn open_recovers_unbookmarked_unsnapshotted_work_from_receipt() -> Result<()> {
    for (colocate, immutable) in [(false, false), (true, true)] {
        let transport = OpenFixture::new(colocate)?;
        let fixture = &transport.fixture;
        fixture.jj(&[
            "--ignore-working-copy",
            "bookmark",
            "delete",
            "unrelated-local-work",
        ])?;
        if immutable {
            fixture.set_immutable_heads(&[&fixture.head])?;
        }
        let old_head = fixture.sha("@")?;
        let output = fixture.run_default(&["--pr", PR_URL, "open"], "")?;
        success(&output);
        let previous = fixture.checkout_previous(&fixture.active_directory()?)?;
        assert_ne!(previous, old_head, "The receipt must include pending edits");
        assert_return_command(&output, &previous);
        assert!(fixture.previous_bookmarks()?.is_empty());
        assert!(!fixture.repo.join("keep local.txt").exists());
        fixture.jj(&["edit", &previous])?;
        assert_eq!(fixture.sha("@")?, previous);
        assert_eq!(
            fs::read_to_string(fixture.repo.join("keep local.txt"))?,
            "Unsnapshotted work\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.repo.join(FILE))?,
            "one\ntwo\nthree\n"
        );
        assert!(fixture.previous_bookmarks()?.is_empty());
    }
    Ok(())
}

#[test]
fn immutable_head_child_supports_full_diff_comments_and_both_review_decisions() -> Result<()> {
    for (colocate, action, event) in [
        (false, "approve", "APPROVE"),
        (true, "request-changes", "REQUEST_CHANGES"),
    ] {
        let transport = OpenFixture::new(colocate)?;
        let fixture = &transport.fixture;
        fixture.set_immutable_heads(&[&fixture.head])?;
        success(&fixture.run_default(&["--pr", PR_URL, "open"], "")?);
        let child = fixture.assert_empty_child(&fixture.head)?;
        let opened = fixture.active_directory()?;
        // Exercise validation both before and after removing the immutable config.
        if !colocate {
            fixture.set_immutable_heads(&[])?;
        }
        success(&fixture.run_default(&["diff"], "")?);
        let session = fixture.active_directory()?;
        assert_ne!(session, opened);
        assert_eq!(fixture.sha("@")?, child);
        let manifest = read_json(&session.join("manifest.json"))?;
        assert_eq!(manifest["pr"]["head_sha"], fixture.head);
        assert_eq!(manifest["pr"]["base_sha"], fixture.base);
        assert_eq!(manifest["merge_base"], transport.common);
        assert_eq!(manifest["files"].as_array().unwrap().len(), 4);
        assert_eq!(
            fs::read_to_string(session.join("base").join(FILE))?,
            "one\ntwo\nthree\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("head").join(FILE))?,
            "ONE\ntwo\nTHREE\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("head/early change.txt"))?,
            "first commit\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("head/late change.txt"))?,
            "second commit\n"
        );
        assert_eq!(
            fs::read_to_string(session.join("base/removed.txt"))?,
            "old\n"
        );
        assert!(!session.join("head/removed.txt").exists());
        for side in ["head", "base"] {
            assert!(!session.join(side).join("base-only.txt").exists());
            assert!(!session.join(side).join("unchanged.txt").exists());
        }
        assert!(
            !fixture
                .git(&["diff", "--name-only", &transport.first, &fixture.head])?
                .lines()
                .any(|path| path == "early change.txt")
        );
        let body = "Review the first commit, not just the empty working copy";
        let mut command = fixture.default_cli(&["comment"]);
        command
            .env("ZED_FILE", session.join("head/early change.txt"))
            .env("ZED_SELECTED_TEXT", "first commit\n");
        success(&fixture.execute(command, body)?);
        let comments = fixture.draft()?["comments"].clone();
        assert_eq!(comments[0]["path"], "early change.txt");
        assert_eq!(comments[0]["body"], body);
        success(&fixture.run_session(&session, &[action], "")?);
        let posts = fixture.posts()?;
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["payload"]["commit_id"], fixture.head);
        assert_ne!(posts[0]["payload"]["commit_id"], child);
        assert_eq!(posts[0]["payload"]["event"], event);
        assert_eq!(posts[0]["payload"]["body"], SUMMARY);
        assert_eq!(
            posts[0]["payload"]["comments"],
            json!([{
                "path": "early change.txt", "line": 1, "side": "RIGHT", "body": body,
            }])
        );
        assert_eq!(posts[0]["drafts_at_post"][0]["comments"], comments);
        let editors = fixture.editors()?;
        assert_eq!(editors.len(), 1);
        let template = editors[0]["initial"].as_str().unwrap();
        assert!(
            template.contains("early change.txt:1–1 RIGHT"),
            "{template}"
        );
        assert!(template.contains("#| > 1 | first commit"), "{template}");
        assert!(template.contains(body), "{template}");
        assert_eq!(fixture.zed_calls()?.len(), 2);
        assert_eq!(fixture.assert_empty_child(&fixture.head)?, child);
        fixture.jj(&["status"])?;
        assert_eq!(fixture.sha("@")?, child);
    }
    Ok(())
}

#[test]
fn open_accepts_global_pr_urls_and_a_single_prompt_line_without_a_positional_argument() -> Result<()>
{
    for (args, input) in [
        (vec!["open", "--pr", PR_URL], "18\n".to_owned()),
        (vec!["open"], " 17 \nnot another PR\n".to_owned()),
        (vec!["open"], format!(" {PR_URL} \n")),
    ] {
        let transport = OpenFixture::new(false)?;
        let fixture = &transport.fixture;
        success(&fixture.run_default(&args, &input)?);
        assert_eq!(fixture.sha("@")?, fixture.head);
        assert_eq!(fixture.zed_calls()?.len(), 1);
        assert!(fixture.previous_bookmarks()?.is_empty());
        assert!(fixture.calls()?.iter().all(|call| call["args"][0] != "pr"));
        assert!(fixture.calls()?.iter().all(|call| call["method"] != "POST"));
    }
    Ok(())
}

#[test]
fn open_and_diff_clearly_refuse_external_session_state() -> Result<()> {
    let fixture = Fixture::new()?;
    for action in ["open", "diff"] {
        for environment_override in [false, true] {
            let mut command = if environment_override {
                fixture.default_cli(&["--pr", PR_URL, action])
            } else {
                fixture.cli(&["--pr", PR_URL, action])
            };
            if environment_override {
                command.env("ZED_PR_REVIEW_STATE_DIR", &fixture.state);
            }
            let output = fixture.execute(command, "")?;
            failure(&output);
            assert!(
                describe(&output).contains("scratch/zed-pr-review"),
                "Explain the required default location: {}",
                describe(&output)
            );
            assert_eq!(fixture.sha("@")?, fixture.head);
            assert!(fixture.zed_calls()?.is_empty());
            assert!(fixture.posts()?.is_empty());
            assert!(!fixture.state.exists());
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct GuardEntry {
    mode: u32,
    modified: std::time::SystemTime,
    contents: Option<Vec<u8>>,
}

fn guard_entries(path: &Path) -> Result<BTreeMap<PathBuf, GuardEntry>> {
    let mut entries = BTreeMap::new();
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    let contents = if metadata.is_symlink() {
        Some(fs::read_link(path)?.as_os_str().as_encoded_bytes().to_vec())
    } else if metadata.is_file() {
        Some(fs::read(path)?)
    } else {
        for entry in fs::read_dir(path)? {
            entries.extend(guard_entries(&entry?.path())?);
        }
        None
    };
    entries.insert(
        path.to_owned(),
        GuardEntry {
            mode: metadata.permissions().mode(),
            modified: metadata.modified()?,
            contents,
        },
    );
    Ok(entries)
}

fn assert_review_guard_refusal(fixture: &Fixture, command: Command) -> Result<Output> {
    let calls = fixture.calls()?;
    let editors = fixture.editors()?;
    let zed = fixture.zed_calls()?;
    let control = fixture.control()?;
    let head = fixture.sha("@")?;
    let paths = [
        fixture.repo.join("scratch"),
        fixture.state.clone(),
        fixture.root.join("tmp"),
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| guard_entries(path))
        .collect::<Result<_>>()?;
    // Cloned descriptors share their offset. Even a read that reaches EOF is
    // observable, without leaving a child blocked on an open pipe.
    let mut stdin = tempfile::tempfile_in(&fixture.root)?;
    stdin.write_all(b"Valid comment body that the guard must not read\n")?;
    stdin.rewind()?;
    let output = bounded_output_with_stdin(command, stdin.try_clone()?, &fixture.root)?;
    failure(&output);
    assert_eq!(
        stdin.stream_position()?,
        0,
        "Guard read stdin: {}",
        describe(&output)
    );
    assert!(
        fixture.calls()? == calls,
        "Guard invoked gh: {}",
        describe(&output)
    );
    assert_eq!(fixture.editors()?, editors, "Guard opened an editor");
    assert_eq!(fixture.zed_calls()?, zed, "Guard opened Zed");
    assert_eq!(fixture.control()?, control);
    for (path, expected) in paths.iter().zip(before) {
        let actual = guard_entries(path)?;
        assert_eq!(
            actual.keys().collect::<Vec<_>>(),
            expected.keys().collect::<Vec<_>>(),
            "Guard created or removed state in {}",
            path.display()
        );
        for (entry, before) in expected {
            assert!(
                actual[&entry] == before,
                "Guard changed {}",
                entry.display()
            );
        }
    }
    assert_eq!(fixture.sha("@")?, head);
    Ok(output)
}

fn assert_review_guard_context(fixture: &Fixture, file: Option<&Path>) -> Result<()> {
    for action in ["comment", "approve", "request-changes"] {
        for overrides in [
            vec![],
            vec!["--pr", PR_URL],
            vec!["--repo", "acme/widgets"],
            vec!["--pr", "17", "--repo", "acme/widgets"],
        ] {
            let mut command = fixture.default_cli(&[action]);
            command.args(overrides);
            if let Some(file) = file {
                command.env("ZED_FILE", file);
            } else {
                command.env_remove("ZED_FILE");
            }
            // default_cli supplies a valid row, column and selected text. A
            // missing selection must never explain these context refusals.
            assert_review_guard_refusal(fixture, command)?;
        }
    }
    Ok(())
}

#[test]
fn review_guard_absent_empty_and_source_context_never_create_state() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    for file in [
        None,
        Some(Path::new("")),
        Some(fixture.repo.join(FILE).as_path()),
    ] {
        assert_review_guard_context(&fixture, file)?;
        assert!(!fixture.state.exists());
        assert!(!fixture.repo.join("scratch").exists());
    }
    Ok(())
}

#[test]
fn review_guard_prompt_state_and_unregistered_paths_never_authorize_reviews() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep this private draft")?;
    // A rejected context must not even recreate an absent workspace lock.
    fs::remove_dir_all(fixture.state.join("workspace"))?;
    let session = fixture.active_directory()?;
    let unknown = session.join("head/unregistered.txt");
    let prompt = fixture.state.join("prompt.txt");
    let outside = fixture.root.join("copied snapshot.txt");
    for path in [&unknown, &prompt, &outside] {
        fs::write(path, "one\ntwo\nthree\n")?;
    }
    for file in [
        session.join("manifest.json"),
        fixture.state.join("active.json"),
        fixture.draft_path()?,
        prompt,
        unknown,
        outside,
        session.join("head/missing.txt"),
        session.join("head/../head").join(FILE),
    ] {
        assert_review_guard_context(&fixture, Some(&file))?;
    }
    Ok(())
}

#[test]
fn review_guard_external_state_override_cannot_authorize_a_registered_snapshot() -> Result<()> {
    let mut fixture = Fixture::review()?;
    let selected = fixture.review_file.clone().unwrap();
    fixture.state = fixture.root.join("external state");
    for action in ["comment", "approve", "request-changes"] {
        for environment in [false, true] {
            let mut command =
                fixture.default_cli(&["--pr", "17", "--repo", "acme/widgets", action]);
            command.env("ZED_FILE", &selected);
            if environment {
                command.env("ZED_PR_REVIEW_STATE_DIR", &fixture.state);
            } else {
                command.arg("--state-dir").arg(&fixture.state);
            }
            assert_review_guard_refusal(&fixture, command)?;
            assert!(!fixture.state.exists());
        }
    }
    Ok(())
}

#[test]
fn review_guard_missing_or_altered_snapshots_and_manifests_reject_before_discovery() -> Result<()> {
    for corruption in [
        "missing snapshot",
        "altered snapshot",
        "missing manifest",
        "altered manifest",
    ] {
        let fixture = Fixture::review()?;
        let selected = fixture.review_file.as_ref().unwrap();
        let manifest = fixture.active_directory()?.join("manifest.json");
        match corruption {
            "missing snapshot" => fs::remove_file(selected)?,
            "altered snapshot" => {
                fs::set_permissions(selected, fs::Permissions::from_mode(0o600))?;
                fs::write(selected, "one\ntwo\naltered elsewhere\n")?;
            }
            "missing manifest" => fs::remove_file(&manifest)?,
            "altered manifest" => {
                fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600))?;
                fs::write(&manifest, "{}\n")?;
            }
            _ => unreachable!(),
        }
        assert_review_guard_context(&fixture, Some(selected))?;
    }
    Ok(())
}

#[test]
fn review_guard_cleaned_head_and_base_reject_all_actions_without_duplicate_posts() -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    success(&fixture.run_session(&directory, &["approve"], "")?);
    assert!(!directory.join("head").exists());
    assert!(!directory.join("base").exists());
    assert!(directory.join("cleanup.json").is_file());
    for side in ["head", "base"] {
        assert_review_guard_context(&fixture, Some(&directory.join(side).join(FILE)))?;
    }
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    Ok(())
}

#[test]
fn review_guard_registered_head_allows_comment_and_both_decisions() -> Result<()> {
    for (action, event) in [
        ("approve", "APPROVE"),
        ("request-changes", "REQUEST_CHANGES"),
    ] {
        let fixture = Fixture::review()?;
        fixture.jj(&["bookmark", "delete", "topic"])?;
        fixture.comment(1, "one", "Comment from the registered head")?;
        let comments = fixture.draft()?["comments"].clone();
        success(&fixture.run(&[action], "")?);
        let posts = fixture.posts()?;
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["payload"]["event"], event);
        assert_eq!(posts[0]["payload"]["commit_id"], fixture.head);
        assert_eq!(posts[0]["payload"]["comments"][0]["path"], FILE);
        assert_eq!(posts[0]["payload"]["comments"][0]["side"], "RIGHT");
        assert_eq!(posts[0]["drafts_at_post"][0]["comments"], comments);
        assert!(!posts[0]["drafts_at_post"][0]["attempt"].is_null());
        assert_eq!(fixture.editors()?.len(), 1);
        assert_review_template(&fixture.editors()?[0], &comments);
        assert!(fixture.calls()?.iter().all(|call| call["args"][0] != "pr"));
        assert!(fixture.active_directory()?.join("cleanup.json").is_file());
    }
    Ok(())
}

#[test]
fn review_guard_registered_base_allows_decisions_but_never_comments() -> Result<()> {
    for (action, event) in [
        ("approve", "APPROVE"),
        ("request-changes", "REQUEST_CHANGES"),
    ] {
        let (fixture, session) = navigation_session()?;
        let file = session.join("base").join(FILE);
        assert_eq!(fs::read_to_string(&file)?, "one\ntwo\nthree\n");
        let mut comment = fixture.default_cli(&["comment"]);
        comment.env("ZED_FILE", &file);
        assert_review_guard_refusal(&fixture, comment)?;
        let mut decision = fixture.default_cli(&[action]);
        decision.env("ZED_FILE", &file);
        for key in ["ZED_ROW", "ZED_COLUMN", "ZED_SELECTED_TEXT"] {
            decision.env_remove(key);
        }
        success(&fixture.execute(decision, "")?);
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.posts()?[0]["payload"]["event"], event);
        assert_eq!(fixture.editors()?.len(), 1);
        assert!(session.join("cleanup.json").is_file());
    }
    Ok(())
}

#[test]
fn default_state_snapshot_comments_preserve_original_files_and_are_ignored_by_jj() -> Result<()> {
    for colocate in [false, true] {
        let mut fixture = Fixture::with_init_args(&[if colocate {
            "--colocate"
        } else {
            "--no-colocate"
        }])?;
        fixture.use_default_state();
        let session = fixture.prepare_session()?;
        success(&fixture.run_session(&session, &["comment"], "Private snapshot comment")?);
        assert_eq!(
            fixture.draft()?["comments"][0],
            json!({
                "path": FILE, "start_line": 1, "line": 1, "column": 1,
                "selected_text": "one", "body": "Private snapshot comment"
            })
        );
        assert_eq!(
            fs::read_to_string(fixture.repo.join(FILE))?,
            "one\ntwo\nthree\n"
        );
        assert_eq!(fixture.active_directory()?, session);
        assert!(fixture.posts()?.is_empty());
        fixture.assert_default_state_ignored()?;
    }
    Ok(())
}

#[test]
fn registered_head_selection_maps_to_repository_filename_and_preserves_context() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    let session = fixture.prepare_session()?;
    fixture.jj(&["bookmark", "delete", "topic"])?;
    fixture.jj(&["bookmark", "create", "zed-pr-review/synthetic-head"])?;
    // The registered snapshot, not the live source file, supplies the selected
    // bytes. Discovery must still leave the recorded jj revision untouched.
    fs::write(fixture.repo.join(FILE), "one\ntwo\nchanged locally\n")?;
    let body = "Literal $HOME and `text`; $(not a command)\nKeep both selected lines.";
    let mut command = fixture.default_cli(&["comment"]);
    command
        .env("ZED_FILE", session.join("head").join(FILE))
        .env("ZED_ROW", "2")
        .env("ZED_COLUMN", "2")
        .env("ZED_SELECTED_TEXT", "wo\nthree\n");
    success(&fixture.execute(command, body)?);
    assert_eq!(
        fixture.draft()?["comments"],
        json!([{
            "path": FILE, "start_line": 2, "line": 3, "column": 2,
            "selected_text": "wo\nthree\n", "body": body
        }])
    );
    assert!(fixture.calls()?.iter().all(|call| call["args"][0] != "pr"));
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    assert_eq!(fixture.active_directory()?, session);
    assert_eq!(fixture.sha("@")?, fixture.head);
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "one\ntwo\nchanged locally\n"
    );
    fs::write(fixture.repo.join(FILE), "one\ntwo\nthree\n")?;
    fixture.assert_default_state_ignored()?;
    Ok(())
}

#[test]
fn review_guard_active_pr_cannot_authorize_original_file_or_missing_context() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep this private draft")?;
    fixture.jj(&["bookmark", "delete", "topic"])?;
    fixture.jj(&["bookmark", "create", "zed-pr-review/synthetic-head"])?;
    assert_eq!(fixture.sha("@")?, fixture.head);
    for file in [
        None,
        Some(Path::new("")),
        Some(fixture.repo.join(FILE).as_path()),
    ] {
        assert_review_guard_context(&fixture, file)?;
    }
    fixture.assert_default_state_ignored()?;
    Ok(())
}

#[test]
fn explicit_pr_mismatch_cannot_rebind_an_active_session_or_its_selection() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    let session = fixture.prepare_session()?;
    success(&fixture.run_session(&session, &["comment"], "Keep this draft")?);
    // PR 18 has the same head/base. A refusal must check session identity, not
    // depend on a missing API fixture or on a coincidental SHA mismatch.
    fixture.configure("additional_pr_numbers", json!([18]))?;
    let before = fixture.draft()?;
    let active = fs::read(fixture.state.join("active.json"))?;
    for action in ["comment", "approve", "request-changes"] {
        for file in [fixture.repo.join(FILE), session.join("head").join(FILE)] {
            for explicit in ["18", "https://github.com/acme/widgets/pull/18"] {
                let mut command = fixture.default_cli(&["--pr", explicit, action]);
                command.env("ZED_FILE", &file);
                let output = fixture.execute(command, "Must not be saved")?;
                failure(&output);

                assert_eq!(fixture.draft()?, before);
                assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
            }
        }
    }
    assert!(fixture.calls()?.iter().all(|call| call["method"] != "POST"));
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn base_unknown_and_other_state_files_cannot_be_used_as_comment_selections() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    // A genuine BASE snapshot, rather than a missing file, must be refused.
    fixture.jj(&["new", "-m", "Modify an existing file"])?;
    fs::write(fixture.repo.join(FILE), "one\ntwo\nTHREE\n")?;
    fixture.jj(&["describe", "-m", "Modified PR head"])?;
    fixture.base = fixture.head.clone();
    fixture.head = fixture.sha("@")?;
    fixture.configure("base", json!(fixture.base))?;
    fixture.configure("head", json!(fixture.head))?;
    fixture.configure(
        "files",
        json!([{
            "filename": FILE, "status": "modified",
            "patch": "@@ -1,3 +1,3 @@\n one\n two\n-three\n+THREE"
        }]),
    )?;
    let session = fixture.prepare_session()?;
    success(&fixture.run_session(&session, &["comment"], "Keep this draft")?);
    let before = fixture.draft()?;
    let unknown = fixture.state.join(FILE);
    fs::write(&unknown, "one\ntwo\nTHREE\n")?;
    for file in [
        session.join("base").join(FILE),
        session.join("manifest.json"),
        fixture.state.join("active.json"),
        fixture.draft_path()?,
        unknown,
    ] {
        let mut command = fixture.default_cli(&["comment"]);
        command.env("ZED_FILE", file);
        failure(&fixture.execute(command, "Must not be saved")?);
        assert_eq!(fixture.draft()?, before);
    }
    let unknown = session.join("head/unregistered.txt");
    fs::write(&unknown, "one\ntwo\nTHREE\n")?;
    let mut command = fixture.default_cli(&["comment"]);
    command.env("ZED_FILE", &unknown);
    failure(&fixture.execute(command, "Same content is not registration")?);
    assert_eq!(fixture.draft()?, before);
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    assert_eq!(fixture.sha("@")?, fixture.head);
    Ok(())
}

#[test]
fn altered_frozen_snapshot_is_rejected_even_when_the_selected_excerpt_still_matches() -> Result<()>
{
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    let session = fixture.prepare_session()?;
    success(&fixture.run_session(&session, &["comment"], "Keep this draft")?);
    let before = fixture.draft()?;
    let file = session.join("head").join(FILE);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    fs::write(&file, "one\ntwo\naltered elsewhere\n")?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400))?;
    let mut command = fixture.default_cli(&["comment"]);
    command.env("ZED_FILE", &file);
    let output = fixture.execute(command, "Must not be saved")?;
    failure(&output);
    assert!(
        describe(&output).to_ascii_lowercase().contains("snapshot"),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.draft()?, before);
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "one\ntwo\nthree\n"
    );
    assert_eq!(fixture.sha("@")?, fixture.head);
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn old_session_tabs_are_not_rebound_after_a_new_checkout_and_session() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    let old = fixture.prepare_session()?;
    let old_manifest = fs::read(old.join("manifest.json"))?;
    let old_file = fs::read(old.join("head").join(FILE))?;
    fixture.jj(&["new", "-m", "A new PR revision with the same selected text"])?;
    fixture.head = fixture.sha("@")?;
    fixture.configure("head", json!(fixture.head))?;
    let current = fixture.prepare_session()?;
    assert_ne!(old, current);
    let mut command = fixture.default_cli(&["comment"]);
    command.env("ZED_FILE", current.join("head").join(FILE));
    success(&fixture.execute(command, "Current session comment")?);
    let before = fixture.draft()?;
    let mut command = fixture.default_cli(&["comment"]);
    command.env("ZED_FILE", old.join("head").join(FILE));
    failure(&fixture.execute(command, "Old tab must not bind to the new PR revision")?);
    assert_eq!(fixture.draft()?, before);
    assert_eq!(fixture.active_directory()?, current);
    assert_eq!(fs::read(old.join("manifest.json"))?, old_manifest);
    assert_eq!(fs::read(old.join("head").join(FILE))?, old_file);
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn registered_session_rejects_checkout_drift_and_checks_remote_drift_at_submission() -> Result<()> {
    for remote_drift in [false, true] {
        let mut fixture = Fixture::new()?;
        fixture.use_default_state();
        let session = fixture.prepare_session()?;
        success(&fixture.run_session(&session, &["comment"], "Keep this draft")?);
        if remote_drift {
            fixture.configure("head", json!(fixture.base))?;
            let calls = fixture.calls()?;
            success(&fixture.run_session(
                &session,
                &["comment"],
                "Still local after remote drift",
            )?);
            assert_eq!(fixture.calls()?, calls);
            assert_eq!(fixture.draft()?["comments"].as_array().unwrap().len(), 2);
        } else {
            fixture.nonempty_child("Unrelated checkout")?;
        }
        let before = fixture.draft()?;
        let actions: &[&str] = if remote_drift {
            &["submit-comment", "approve", "request-changes"]
        } else {
            &["comment", "submit-comment", "approve", "request-changes"]
        };
        for action in actions {
            let calls_before = fixture.calls()?.len();
            let mut command = fixture.default_cli(&[action]);
            command.env("ZED_FILE", session.join("head").join(FILE));
            let output = fixture.execute(command, "Must not be saved")?;
            failure(&output);
            let message = describe(&output).to_ascii_lowercase();
            assert!(
                message.contains("head") || message.contains("stale"),
                "{message}"
            );
            if remote_drift {
                assert!(
                    fixture.calls()?.len() > calls_before,
                    "Submission must still check GitHub"
                );
            }
            assert_eq!(fixture.draft()?, before);
        }
        assert!(fixture.posts()?.is_empty());
        assert!(fixture.editors()?.is_empty());
        assert_eq!(fixture.active_directory()?, session);
    }
    Ok(())
}

fn assert_checkout_guard_rejects_review_actions(fixture: &Fixture) -> Result<()> {
    let checkout = fixture.sha("@")?;
    let contents = fs::read(fixture.repo.join(FILE))?;
    let state = guard_entries(&fixture.state)?;
    for action in [
        "diff",
        "comment",
        "approve",
        "request-changes",
        "stack",
        "sb",
        "sf",
    ] {
        let output = fixture.run(&["--pr", PR_URL, action], "Must not be saved")?;
        failure(&output);
        assert!(
            describe(&output).contains("jj @"),
            "{action}: {}",
            describe(&output)
        );
        assert_eq!(
            guard_entries(&fixture.state)?,
            state,
            "{action} changed review state"
        );
    }
    for args in [
        vec!["--reviewer", "alice"],
        vec!["--pr", PR_URL, "--reviewer", "alice"],
        vec![
            "--pr",
            "17",
            "--repo",
            "github.com/acme/widgets",
            "--reviewer",
            "alice",
        ],
    ] {
        let output = fixture.run_ready(&args)?;
        failure(&output);
        assert!(
            describe(&output).contains("jj @"),
            "{args:?}: {}",
            describe(&output)
        );
        assert_eq!(guard_entries(&fixture.state)?, state);
    }
    assert_eq!(fixture.sha("@")?, checkout);
    assert_eq!(fs::read(fixture.repo.join(FILE))?, contents);
    fixture.assert_ready_mutations(&[])?;
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    assert!(fixture.zed_calls()?.is_empty());
    Ok(())
}

#[test]
fn checkout_guard_rejects_an_empty_grandchild() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the original PR draft")?;
    let child = fixture.empty_immutable_head_child()?;
    fixture.jj(&["new", "-m", "Empty grandchild is not the PR checkout"])?;
    let grandchild = fixture.sha("@")?;
    assert_eq!(fixture.sha("@ & empty() & ~conflicts()")?, grandchild);
    assert_eq!(fixture.sha("@-")?, child);
    assert_eq!(fixture.sha("@--")?, fixture.head);
    assert_checkout_guard_rejects_review_actions(&fixture)
}

#[test]
fn checkout_guard_rejects_an_unrelated_commit_with_the_same_tree() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the original PR draft")?;
    fixture.jj(&["new", &fixture.base, "-m", "Unrelated copy of the PR tree"])?;
    fixture.jj(&["restore", "--from", &fixture.head, FILE])?;
    let unrelated = fixture.sha("@")?;
    fixture.jj(&["new", "-m", "Empty child of unrelated history"])?;
    let checkout = fixture.sha("@")?;
    assert_eq!(fixture.sha("@ & empty() & ~conflicts()")?, checkout);
    assert_eq!(fixture.sha("@-")?, unrelated);
    assert!(fixture.sha(&format!("@ & {}::", fixture.head))?.is_empty());
    assert!(
        fixture
            .git(&["diff", "--name-only", &fixture.head, &checkout])?
            .is_empty()
    );
    assert_checkout_guard_rejects_review_actions(&fixture)
}

fn checkout_guard_conflicted_merge(fixture: &Fixture) -> Result<String> {
    fixture.jj(&[
        "new",
        &fixture.base,
        "-m",
        "Conflicting addition on another branch",
    ])?;
    fs::write(
        fixture.repo.join(FILE),
        "Conflicting content from another branch\n",
    )?;
    fixture.jj(&["describe", "-m", "Recorded conflicting addition"])?;
    let other = fixture.sha("@")?;
    fixture.jj(&["new", &fixture.head, &other, "-m", "Unresolved merge"])?;
    let merge = fixture.sha("@")?;
    assert_eq!(fixture.sha("@ & conflicts()")?, merge);
    Ok(merge)
}

#[test]
fn checkout_guard_rejects_a_conflicted_single_parent_child() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the original PR draft")?;
    let merge = checkout_guard_conflicted_merge(&fixture)?;
    fixture.jj(&[
        "new",
        &fixture.head,
        "-m",
        "Conflicted child of the PR head",
    ])?;
    fixture.jj(&["restore", "--from", &merge, FILE])?;
    let child = fixture.sha("@")?;
    assert_eq!(fixture.sha("@ & conflicts()")?, child);
    assert_eq!(fixture.sha("@-")?, fixture.head);
    assert_checkout_guard_rejects_review_actions(&fixture)
}

#[test]
fn checkout_guard_rejects_an_empty_conflicted_child_of_the_pr_head() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.head = checkout_guard_conflicted_merge(&fixture)?;
    fixture.configure("head", json!(fixture.head))?;
    fixture.jj(&["bookmark", "set", "topic", "-r", &fixture.head])?;
    fixture.jj(&["new", "-m", "Empty child inherits the PR head's conflict"])?;
    let child = fixture.sha("@")?;
    assert_eq!(fixture.sha("@ & empty() & conflicts()")?, child);
    assert_eq!(fixture.sha("@-")?, fixture.head);
    // Without the conflict guard, this would qualify for the parent fallback.
    failure(&fixture.run_ready(&[])?);
    assert!(fixture.calls()?.is_empty());
    let output = fixture.run_ready(&["--pr", PR_URL, "--reviewer", "alice"])?;
    failure(&output);
    assert!(describe(&output).contains("jj @"), "{}", describe(&output));
    assert_eq!(fixture.sha("@")?, child);
    fixture.assert_ready_mutations(&[])?;
    fixture.assert_stateless_ready()
}

#[test]
fn checkout_guard_rejects_an_empty_merge_containing_the_pr_head() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the original PR draft")?;
    fixture.jj(&[
        "new",
        &fixture.base,
        "-m",
        "Independent copy of the PR tree",
    ])?;
    fixture.jj(&["restore", "--from", &fixture.head, FILE])?;
    let other = fixture.sha("@")?;
    fixture.jj(&[
        "new",
        &fixture.head,
        &other,
        "-m",
        "Empty merge of identical trees",
    ])?;
    let merge = fixture.sha("@")?;
    assert_eq!(fixture.sha("@ & empty() & ~conflicts()")?, merge);
    let parents = fixture.sha("@-")?;
    assert_eq!(parents.len(), 80, "The merge must have exactly two parents");
    assert!(parents.contains(&fixture.head) && parents.contains(&other));
    assert!(
        fixture
            .git(&["diff", "--name-only", &fixture.head, &merge])?
            .is_empty()
    );
    assert_checkout_guard_rejects_review_actions(&fixture)
}

#[test]
fn diff_creates_frozen_sessions_without_changing_checkout_or_reusing_old_tabs() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    success(&fixture.run_default(&["diff"], "")?);
    let first = fixture.active_directory()?;
    let old_manifest = fs::read(first.join("manifest.json"))?;
    let old_head = fs::read(first.join("head").join(FILE))?;
    fixture.jj(&["bookmark", "delete", "topic"])?;
    success(&fixture.run_default(&["diff"], "")?);
    let second = fixture.active_directory()?;
    assert_ne!(first, second);
    assert_eq!(fs::read(first.join("manifest.json"))?, old_manifest);
    assert_eq!(fs::read(first.join("head").join(FILE))?, old_head);
    let launches = fixture.zed_calls()?;
    assert_eq!(launches.len(), 2);
    for (launch, session) in launches.iter().zip([first, second]) {
        assert_eq!(launch["cwd"], json!(fixture.repo));
        assert_eq!(
            launch["args"],
            json!([
                "--existing",
                "--diff",
                session.join("base"),
                session.join("head")
            ])
        );
        assert!(
            launch["checkouts"].as_object().unwrap().is_empty(),
            "diff does not check out another revision"
        );
    }
    assert_eq!(fixture.sha("@")?, fixture.head);
    assert!(fixture.posts()?.is_empty());
    fixture.assert_default_state_ignored()?;
    Ok(())
}

#[test]
fn comments_are_local_and_preserve_multiline_selection_and_literal_body() -> Result<()> {
    let fixture = Fixture::review()?;
    let body = "Literal $(touch injected-comment), `touch injected-backticks`; $HOME & |\nSecond paragraph: 'quoted' and \\\"double\\\".";
    let calls = fixture.calls()?;
    fixture.comment(1, "one\ntwo\n", body)?;
    fixture.comment(3, "three\n", "Last line")?;
    assert_eq!(fixture.calls()?, calls, "Local comments must not invoke gh");
    let draft = fixture.draft()?;
    assert_eq!(draft["version"], 1);
    assert_eq!(draft["pr"]["head_sha"], fixture.head);
    assert_eq!(draft["pr"]["base_sha"], fixture.base);
    assert_eq!(
        draft["comments"],
        json!([
            {"path": FILE, "start_line": 1, "line": 2, "column": 1,
             "selected_text": "one\ntwo\n", "body": body},
            {"path": FILE, "start_line": 3, "line": 3, "column": 1,
             "selected_text": "three\n", "body": "Last line"},
        ])
    );
    assert!(draft["attempt"].is_null());
    assert_eq!(draft["retry_history"], json!([]));
    assert!(draft["submitted"].is_null());
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    assert!(!fixture.repo.join("injected-comment").exists());
    assert!(!fixture.repo.join("injected-backticks").exists());
    assert_eq!(
        fixture.sha("@")?,
        fixture.head,
        "Discovery must not snapshot jj @"
    );
    let path = fixture.draft_path()?;
    for private in [&fixture.state, path.parent().unwrap(), &path] {
        assert_eq!(fs::metadata(private)?.permissions().mode() & 0o077, 0);
    }
    Ok(())
}

#[test]
fn cached_comments_work_offline_and_leave_remote_drift_checks_to_submission() -> Result<()> {
    let fixture = Fixture::review()?;
    let session = fixture.active_directory()?;
    let manifest = read_json(&session.join("manifest.json"))?;
    assert_eq!(manifest["patches_cached"], true);
    let snapshots = CleanupSession::capture(&session)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    let original = fixture.control()?;
    for (index, (field, value)) in [
        ("deny_discovery", json!(true)),
        ("reviewer", json!("another-account")),
        ("head", json!(fixture.base)),
        ("base", json!(fixture.head)),
        ("pr_state", json!("closed")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut control = original.clone();
        control[field] = value;
        fixture.set_control(control.clone())?;
        let calls = fixture.calls()?;
        let body = format!("Local comment with {field} changed");
        fixture.comment(1, "one\ntwo\n", &body)?;
        assert_eq!(fixture.calls()?, calls, "{field} must not trigger gh");
        assert_eq!(fixture.control()?, control);
        let draft = fixture.draft()?;
        assert_eq!(draft["pr"], manifest["pr"]);
        assert_eq!(draft["comments"].as_array().unwrap().len(), index + 1);
        assert_eq!(draft["comments"][index]["body"], body);
        let bytes = fs::read(fixture.draft_path()?)?;
        for action in ["submit-comment", "approve", "request-changes"] {
            failure(&fixture.run(&[action], "")?);
            assert_eq!(fs::read(fixture.draft_path()?)?, bytes);
            assert!(fixture.posts()?.is_empty());
            assert!(fixture.editors()?.is_empty());
        }
        assert!(fixture.calls()?.len() > calls.len());
        assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
        snapshots.assert_kept()?;
        assert_eq!(fixture.sha("@")?, fixture.head);
    }
    assert_eq!(
        fixture.drafts()?.len(),
        1,
        "Keep the pinned reviewer's draft"
    );
    Ok(())
}

#[test]
fn cached_comments_read_frozen_bytes_without_git_blobs_or_state_scans() -> Result<()> {
    let (fixture, session) = navigation_session()?;
    let snapshots = CleanupSession::capture(&session)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    // A recursive prepare_directory scan would reject this unrelated entry.
    let unrelated = fixture.state.join("unrelated-private-state");
    symlink(fixture.root.join("home"), &unrelated)?;
    fs::write(fixture.repo.join(FILE), "Unsnapshotted local work\n")?;
    fixture.configure("deny_discovery", json!(true))?;
    fixture.configure("files", json!([]))?;
    navigation_clear_calls(&fixture)?;
    let mut command = fixture.default_cli(&["comment"]);
    command
        .env("ZED_FILE", session.join("head").join(FILE))
        .env("ZED_ROW", "2")
        .env("ZED_COLUMN", "2")
        .env("ZED_SELECTED_TEXT", "wo\nTHREE\n");
    success(&fixture.execute(command, "Only frozen bytes and cached patches")?);
    assert_eq!(
        fixture.draft()?["comments"][1],
        json!({
            "path": FILE, "start_line": 2, "line": 3, "column": 2,
            "selected_text": "wo\nTHREE\n", "body": "Only frozen bytes and cached patches"
        })
    );
    assert!(fixture.calls()?.is_empty());
    for call in navigation_calls(&fixture)? {
        let args = call["args"].as_array().unwrap();
        if call["program"] == "git" {
            for forbidden in [
                "show",
                "cat-file",
                "ls-files",
                "ls-tree",
                "check-ignore",
                "fetch",
            ] {
                assert!(
                    !args.iter().any(|arg| arg == forbidden),
                    "Unexpected Git read: {call}"
                );
            }
        } else if call["program"] == "jj" {
            assert!(
                !args.iter().any(|arg| arg == "file"),
                "Unexpected jj file scan: {call}"
            );
        }
    }
    assert_eq!(fixture.sha("@")?, fixture.head);
    assert_eq!(
        fs::read_to_string(fixture.repo.join(FILE))?,
        "Unsnapshotted local work\n"
    );
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    assert_eq!(fs::read_link(unrelated)?, fixture.root.join("home"));
    snapshots.assert_kept()?;
    Ok(())
}

#[test]
fn cached_comments_require_a_cached_rhs_hunk_even_when_live_files_allow_the_selection() -> Result<()>
{
    let fixture = navigation_fixture()?;
    let live_files = fixture.control()?["files"].clone();
    fixture.configure(
        "files",
        json!([{
            "filename": FILE, "status": "modified", "patch": "@@ -3 +3 @@\n-three\n+THREE"
        }]),
    )?;
    let session = fixture.prepare_session()?;
    fixture.configure("files", live_files)?;
    fixture.configure("deny_discovery", json!(true))?;
    let calls = fixture.calls()?;
    let mut command = fixture.default_cli(&["comment"]);
    command
        .env("ZED_FILE", session.join("head").join(FILE))
        .env("ZED_ROW", "3")
        .env("ZED_SELECTED_TEXT", "THREE");
    success(&fixture.execute(command, "Cached changed line")?);
    let before = fs::read(fixture.draft_path()?)?;
    let output = fixture.run_session(&session, &["comment"], "Outside the cached hunk")?;
    failure(&output);
    assert!(describe(&output).contains("hunk"), "{}", describe(&output));
    assert_eq!(fs::read(fixture.draft_path()?)?, before);
    assert_eq!(fixture.calls()?, calls);
    Ok(())
}

#[test]
fn cached_comments_do_not_fetch_missing_or_empty_patches() -> Result<()> {
    for patch in [Value::Null, json!("")] {
        let mut fixture = Fixture::new()?;
        fixture.use_default_state();
        fixture.configure(
            "files",
            json!([{
                "filename": FILE, "status": "added", "patch": patch
            }]),
        )?;
        let session = fixture.prepare_session()?;
        assert_eq!(
            read_json(&session.join("manifest.json"))?["patches_cached"],
            true
        );
        fixture.configure("deny_discovery", json!(true))?;
        let calls = fixture.calls()?;
        let output = fixture.run_session(&session, &["comment"], "No cached patch")?;
        failure(&output);
        assert!(describe(&output).contains("patch"), "{}", describe(&output));
        assert_eq!(fixture.calls()?, calls);
        fixture.assert_no_comments()?;
    }
    Ok(())
}

#[test]
fn cached_comments_reject_explicit_context_mismatches_without_api_calls() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the pinned draft")?;
    fixture.configure("deny_discovery", json!(true))?;
    let before = fs::read(fixture.draft_path()?)?;
    let active = fs::read(fixture.state.join("active.json"))?;
    let calls = fixture.calls()?;
    for args in [
        vec!["--pr", "18"],
        vec!["--pr", "https://github.com/acme/widgets/pull/18"],
        vec!["--pr", "https://github.com/acme/other/pull/17"],
        vec!["--pr", "https://other.example/acme/widgets/pull/17"],
        vec!["--repo", "acme/other"],
        vec!["--repo", "other.example/acme/widgets"],
        vec!["--pr", PR_URL, "--repo", "acme/other"],
        vec!["--pr", "17", "--repo", "acme/other"],
    ] {
        let mut command = fixture.cli(&["comment"]);
        command.args(&args);
        navigation_failure(&fixture.execute(command, "Must not rebind the session")?);
        assert_eq!(fixture.calls()?, calls, "{args:?}");
        assert_eq!(fs::read(fixture.draft_path()?)?, before);
        assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    }
    Ok(())
}

#[test]
fn cached_comments_reject_stale_recorded_checkout_offline() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the original draft")?;
    let checkout = fixture.nonempty_child("Recorded work outside the pinned PR")?;
    fixture.configure("deny_discovery", json!(true))?;
    let before = fs::read(fixture.draft_path()?)?;
    let calls = fixture.calls()?;
    let output = fixture.run(&["comment"], "Must not use a stale session")?;
    failure(&output);
    assert!(describe(&output).contains("jj @"), "{}", describe(&output));
    assert_eq!(fixture.calls()?, calls);
    assert_eq!(fs::read(fixture.draft_path()?)?, before);
    assert_eq!(fixture.sha("@")?, checkout);
    Ok(())
}

#[test]
fn cached_comments_require_editable_drafts_with_matching_local_pins() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep this comment")?;
    fixture.configure("deny_discovery", json!(true))?;
    let original = fixture.draft()?;
    let path = fixture.draft_path()?;
    let calls = fixture.calls()?;
    let attempt = json!({"payload": {}, "previous_review_ids": []});
    let mut invalid = Vec::new();
    for (field, value) in [
        ("head_sha", json!(fixture.base)),
        ("base_sha", json!(fixture.head)),
        ("reviewer", json!("another-account")),
        ("repo", json!("acme/other")),
        ("host", json!("other.example")),
        ("number", json!(18)),
        ("head_repo", json!("contributor/widgets")),
        ("head_ref", json!("another-branch")),
    ] {
        let mut draft = original.clone();
        draft["pr"][field] = value;
        invalid.push(draft);
    }
    for (field, value) in [
        ("attempt", attempt),
        (
            "submitted",
            json!({"id": 501, "url": format!("{PR_URL}#pullrequestreview-501"), "state": "APPROVED"}),
        ),
    ] {
        let mut draft = original.clone();
        draft[field] = value;
        invalid.push(draft);
    }
    for draft in invalid {
        let bytes = serde_json::to_vec(&draft)?;
        fs::write(&path, &bytes)?;
        failure(&fixture.run(&["comment"], "Must not change a pinned or finalized draft")?);
        assert_eq!(fs::read(&path)?, bytes);
        assert_eq!(fixture.calls()?, calls);
    }
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn legacy_comments_fetch_files_and_still_require_a_live_matching_pr() -> Result<()> {
    let fixture = Fixture::review()?;
    let session = fixture.active_directory()?;
    let path = session.join("manifest.json");
    let mut manifest = read_json(&path)?;
    manifest.as_object_mut().unwrap().remove("patches_cached");
    for file in manifest["files"].as_array_mut().unwrap() {
        file.as_object_mut().unwrap().remove("patch");
    }
    let bytes = serde_json::to_vec(&manifest)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    fs::write(&path, &bytes)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
    let active_path = fixture.state.join("active.json");
    let mut active = read_json(&active_path)?;
    active["manifest_sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
    fs::write(&active_path, serde_json::to_vec(&active)?)?;
    let calls_before = fixture.calls()?.len();
    fixture.comment(1, "one", "Legacy session comment")?;
    let calls = fixture.calls()?;
    assert!(
        calls[calls_before..]
            .iter()
            .any(|call| call["endpoint"] == PR_ENDPOINT)
    );
    assert!(
        calls[calls_before..]
            .iter()
            .any(|call| call["endpoint"] == format!("{PR_ENDPOINT}/files?per_page=100"))
    );
    let before = fs::read(fixture.draft_path()?)?;
    let original = fixture.control()?;
    for (field, value) in [
        ("files", json!([])),
        ("head", json!(fixture.base)),
        ("reviewer", json!("another-account")),
        ("deny_discovery", json!(true)),
    ] {
        let mut control = original.clone();
        control[field] = value;
        fixture.set_control(control)?;
        let calls_before = fixture.calls()?.len();
        failure(&fixture.run(&["comment"], "Legacy sessions need live validation")?);
        assert!(fixture.calls()?.len() > calls_before);
        assert_eq!(fs::read(fixture.draft_path()?)?, before);
    }
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn saved_drafts_without_retry_history_load_with_an_empty_history() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Legacy comment")?;
    let mut legacy = fixture.draft()?;
    legacy.as_object_mut().unwrap().remove("retry_history");
    fs::write(fixture.draft_path()?, serde_json::to_vec(&legacy)?)?;
    fixture.configure("deny_discovery", json!(true))?;
    let calls_before = fixture.calls()?.len();
    success(&fixture.saved(&["list"], "")?);
    fixture.configure("editor_body", json!("Updated legacy comment"))?;
    success(&fixture.saved(&["edit", "1"], "")?);
    let updated = fixture.draft()?;
    assert_eq!(updated["retry_history"], json!([]));
    assert_eq!(updated["comments"][0]["body"], "Updated legacy comment");
    assert_eq!(updated["pr"], legacy["pr"]);
    assert!(updated["attempt"].is_null());
    assert!(updated["submitted"].is_null());
    assert_eq!(fixture.calls()?.len(), calls_before);
    Ok(())
}

#[test]
fn explicit_pr_url_and_number_work_without_bookmark_discovery() -> Result<()> {
    for args in [
        vec!["--pr", PR_URL, "comment"],
        vec!["--pr", "17", "--repo", "acme/widgets", "comment"],
        vec!["--pr", "17", "--repo", "github.com/acme/widgets", "comment"],
        vec!["--repo", "acme/widgets", "comment"],
    ] {
        let fixture = Fixture::review()?;
        fixture.jj(&["bookmark", "delete", "topic"])?;
        fixture.configure("deny_discovery", json!(true))?;
        let calls = fixture.calls()?;
        success(&fixture.run(&args, "Explicit PR")?);
        assert_eq!(fixture.calls()?, calls);
        assert_eq!(fixture.draft()?["comments"][0]["body"], "Explicit PR");
        assert!(fixture.calls()?.iter().all(|call| call["args"][0] == "api"));
        assert!(fixture.posts()?.is_empty());
    }
    Ok(())
}

#[test]
fn blank_stdin_cancels_without_changing_an_existing_comment() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep me")?;
    let before = fs::read(fixture.draft_path()?)?;
    fixture.configure("deny_discovery", json!(true))?;
    let calls = fixture.calls()?;
    for input in ["", " \t\n\n"] {
        let output = fixture.run(&["comment"], input)?;
        success(&output);
        assert!(
            describe(&output).contains("cancelled"),
            "{}",
            describe(&output)
        );
        assert_eq!(fs::read(fixture.draft_path()?)?, before);
        assert_eq!(fixture.calls()?, calls);
    }
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn missing_empty_and_inexact_selections_are_rejected() -> Result<()> {
    let fixture = Fixture::review()?;
    for selected in [None, Some(""), Some(" \n\t"), Some("not the selected text")] {
        let mut command = fixture.cli(&["comment"]);
        if let Some(text) = selected {
            command.env("ZED_SELECTED_TEXT", text);
        } else {
            command.env_remove("ZED_SELECTED_TEXT");
        }
        failure(&fixture.execute(command, "Must not be saved")?);
        fixture.assert_no_comments()?;
    }
    Ok(())
}

#[test]
fn a_changed_local_file_is_rejected_without_snapshotting() -> Result<()> {
    let fixture = Fixture::new()?;
    // The selected excerpt still matches; bytes elsewhere in the file have changed.
    fs::write(fixture.repo.join(FILE), "one\ntwo\nchanged locally\n")?;
    failure(&fixture.run(&["comment"], "Must not be saved")?);
    fixture.assert_no_comments()?;
    assert_eq!(fixture.sha("@")?, fixture.head);
    Ok(())
}

#[test]
fn a_remote_pr_head_that_differs_from_jj_at_does_not_block_local_comments() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.configure("head", json!(fixture.base))?;
    let calls = fixture.calls()?;
    success(&fixture.run(&["--pr", PR_URL, "comment"], "Pinned revision")?);
    assert_eq!(fixture.calls()?, calls);
    assert_eq!(fixture.draft()?["pr"]["head_sha"], fixture.head);
    assert_eq!(fixture.draft()?["comments"][0]["body"], "Pinned revision");
    assert!(fixture.posts()?.is_empty());
    assert_eq!(fixture.sha("@")?, fixture.head);
    Ok(())
}

#[test]
fn a_state_directory_with_a_symlink_ancestor_inside_the_repo_is_rejected() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let alias = fixture.root.join("checkout alias");
    symlink(&fixture.repo, &alias)?;
    fixture.state = alias.join("nested/private state");
    assert!(!fixture.state.starts_with(&fixture.repo));
    // Local draft commands retain their original state-directory validation.
    let output = fixture.run(&["list"], "")?;
    failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("A symlink alias into the repository is not a safe state directory"),
        "{}",
        describe(&output)
    );
    assert!(!fixture.repo.join("nested").exists());
    assert!(fixture.drafts()?.is_empty());
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    assert_eq!(fixture.sha("@")?, fixture.head);
    Ok(())
}

#[test]
fn own_pr_cannot_be_approved_or_have_changes_requested() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.configure("author", json!("ReViEwEr"))?;
    fixture.comment(1, "one", "Keep the local comment")?;
    let before = fixture.draft()?;
    for action in ["approve", "request-changes"] {
        let output = fixture.run(&[action], "")?;
        failure(&output);
        assert!(String::from_utf8_lossy(&output.stderr).contains("your own PR"));
        assert_eq!(fixture.draft()?, before);
    }
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn an_existing_pending_review_blocks_both_actions_without_changing_the_draft() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep the local comment")?;
    let before = fixture.draft()?;
    let reviews = json!([{
        "id": 90, "state": "PENDING", "user": {"login": "ReViEwEr"},
        "body": "Unrelated pending review", "commit_id": fixture.head,
    }]);
    fixture.configure("reviews", reviews.clone())?;
    for action in ["approve", "request-changes"] {
        let output = fixture.run(&[action], "")?;
        failure(&output);
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending review"));
        assert_eq!(fixture.draft()?, before);
    }
    assert_eq!(fixture.control()?["reviews"], reviews);
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn another_users_pending_review_does_not_block_submission() -> Result<()> {
    let fixture = Fixture::review()?;
    let pending = json!({
        "id": 90, "state": "PENDING", "user": {"login": "another-user"},
        "body": "Unrelated pending review", "commit_id": fixture.head,
    });
    fixture.configure("reviews", json!([pending]))?;
    success(&fixture.run(&["approve"], "")?);
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(
        fixture.posts()?[0]["drafts_at_post"][0]["attempt"]["previous_review_ids"],
        json!([90])
    );
    assert_eq!(fixture.control()?["reviews"][0], pending);
    assert_eq!(fixture.draft()?["submitted"]["state"], "APPROVED");
    Ok(())
}

#[test]
fn both_actions_open_an_empty_editor_without_comments_and_abort_whitespace() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.configure("editor_body", json!(" \t\n"))?;
    for action in ["approve", "request-changes"] {
        success(&fixture.run(&[action], "This stdin must not become the review body")?);
        fixture.assert_no_comments()?;
    }
    let editors = fixture.editors()?;
    assert_eq!(editors.len(), 2);
    assert!(editors.iter().all(|editor| editor["initial"] == ""));
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn both_actions_abort_when_only_review_references_remain() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(
        1,
        "one\ntwo\n",
        "First comment\n\nMore detail\n#| Literal comment text",
    )?;
    fixture.comment(3, "three", "Second comment\n  Indented detail")?;
    let before = fixture.draft()?;
    // Reference code must come from the pinned head, not unsaved working-copy changes.
    fs::write(
        fixture.repo.join(FILE),
        "local one\nlocal two\nlocal three\n",
    )?;
    for action in ["approve", "request-changes"] {
        for (mode, body) in [
            ("unchanged", "This configured body must not be written"),
            ("prepend", " \t\n\n"),
        ] {
            fixture.configure("editor_mode", json!(mode))?;
            fixture.configure("editor_body", json!(body))?;
            success(&fixture.run(&[action], "This stdin must not become the review body")?);
            assert_eq!(fixture.draft()?, before, "{action}: {mode}");
            assert!(fixture.posts()?.is_empty());
        }
    }
    let editors = fixture.editors()?;
    assert_eq!(editors.len(), 4);
    for editor in editors {
        assert_review_template(&editor, &before["comments"]);
    }
    assert_eq!(
        fixture.sha("@")?,
        fixture.head,
        "Reading references must not snapshot jj @"
    );
    Ok(())
}

#[test]
fn both_actions_submit_only_the_summary_and_keep_original_inline_comments() -> Result<()> {
    let first_body = "First comment\n\nMore detail\n#| Literal comment text";
    let second_body = "Second comment\n  Indented detail";
    let summary = format!("# Heading\n\n{SUMMARY}\n\n## Follow-up\nKeep ordinary Markdown.");
    let editor_body = format!(
        "# Heading\n  #| Omit this indented reference\n\n{SUMMARY}\n\t#|also omit without a separating space\n#|\n\n## Follow-up\nKeep ordinary Markdown.\n"
    );
    for (action, event, state) in [
        ("approve", "APPROVE", "APPROVED"),
        ("request-changes", "REQUEST_CHANGES", "CHANGES_REQUESTED"),
    ] {
        let fixture = Fixture::review()?;
        fixture.comment(1, "one\ntwo\n", first_body)?;
        fixture.comment(3, "three", second_body)?;
        let before = fixture.draft()?;
        fixture.configure("editor_mode", json!("prepend"))?;
        fixture.configure("editor_body", json!(editor_body))?;
        success(&fixture.run(&[action], "ignored stdin")?);
        let editors = fixture.editors()?;
        assert_eq!(editors.len(), 1);
        assert_review_template(&editors[0], &before["comments"]);
        let posts = fixture.posts()?;
        assert_eq!(posts.len(), 1);
        let payload = json!({
            "event": event, "commit_id": fixture.head, "body": summary,
            "comments": [
                {"path": FILE, "start_line": 1, "line": 2, "start_side": "RIGHT", "side": "RIGHT", "body": first_body},
                {"path": FILE, "line": 3, "side": "RIGHT", "body": second_body},
            ],
        });
        assert_eq!(posts[0]["payload"], payload);
        assert_eq!(posts[0]["drafts_at_post"][0]["attempt"]["payload"], payload);
        assert_eq!(
            posts[0]["drafts_at_post"][0]["comments"],
            before["comments"]
        );
        let submitted = fixture.draft()?;
        assert_eq!(submitted["comments"], before["comments"]);
        assert_eq!(submitted["submitted"]["state"], state);
    }
    Ok(())
}

#[test]
fn editing_review_references_does_not_change_the_saved_comment() -> Result<()> {
    for action in ["approve", "request-changes"] {
        let fixture = Fixture::review()?;
        fixture.comment(1, "one\ntwo\n", "Original body\nSecond paragraph")?;
        let before = fixture.draft()?;
        fixture.configure("editor_mode", json!("unchanged"))?;
        success(&fixture.run(&[action], "")?);
        assert_eq!(fixture.draft()?, before);
        assert!(fixture.posts()?.is_empty());
        let editors = fixture.editors()?;
        assert_eq!(editors.len(), 1);
        assert_review_template(&editors[0], &before["comments"]);
        let edited_reference = editors[0]["initial"]
            .as_str()
            .unwrap()
            .replace(FILE, "not a real path.txt")
            .replace("RIGHT", "LEFT")
            .replace("one", "edited source")
            .replace("Original body", "Edited reference, not a new comment");
        fixture.configure("editor_mode", json!("write"))?;
        fixture.configure("editor_body", json!(edited_reference))?;
        success(&fixture.run(&[action], "")?);
        assert_eq!(fixture.draft()?, before);
        assert!(fixture.posts()?.is_empty());

        fixture.configure("editor_body", json!(format!("{SUMMARY}{edited_reference}")))?;
        success(&fixture.run(&[action], "")?);
        let posts = fixture.posts()?;
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["payload"]["body"], SUMMARY);
        assert_eq!(
            posts[0]["payload"]["comments"],
            json!([
                {"path": FILE, "start_line": 1, "line": 2, "start_side": "RIGHT", "side": "RIGHT", "body": "Original body\nSecond paragraph"},
            ])
        );
        assert_eq!(
            posts[0]["drafts_at_post"][0]["comments"],
            before["comments"]
        );
        assert_eq!(fixture.draft()?["comments"], before["comments"]);
    }
    Ok(())
}

#[test]
fn editor_failure_preserves_comments_and_never_posts() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Original body")?;
    let before = fixture.draft()?;
    fixture.configure("editor_mode", json!("fail"))?;
    fixture.configure("editor_body", json!("Do not save these editor changes"))?;
    for args in [vec!["approve"], vec!["request-changes"], vec!["edit", "1"]] {
        failure(&fixture.run(&args, "")?);
        assert_eq!(fixture.draft()?, before);
    }
    let editors = fixture.editors()?;
    assert_eq!(editors.len(), 3);
    assert_review_template(&editors[0], &before["comments"]);
    assert_review_template(&editors[1], &before["comments"]);
    assert_eq!(editors[2]["initial"], "Original body");
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn approval_posts_one_batch_after_persisting_attempt_and_blocks_duplicates() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one\ntwo\n", "First comment")?;
    fixture.comment(3, "three", "Second comment")?;
    fixture.configure(
        "reviews",
        json!([{
            "id": 90, "state": "COMMENTED", "user": {"login": "someone-else"},
            "body": "Older review", "commit_id": fixture.head,
        }]),
    )?;
    let before = fixture.draft()?;
    success(&fixture.run(&["approve"], "ignored stdin")?);
    let posts = fixture.posts()?;
    assert_eq!(posts.len(), 1);
    let payload = json!({
        "event": "APPROVE", "commit_id": fixture.head, "body": SUMMARY,
        "comments": [
            {"path": FILE, "start_line": 1, "line": 2, "start_side": "RIGHT", "side": "RIGHT", "body": "First comment"},
            {"path": FILE, "line": 3, "side": "RIGHT", "body": "Second comment"},
        ],
    });
    assert_eq!(posts[0]["payload"], payload);
    assert_eq!(posts[0]["drafts_at_post"].as_array().unwrap().len(), 1);
    let at_post = &posts[0]["drafts_at_post"][0];
    assert_eq!(at_post["attempt"]["payload"], payload);
    assert_eq!(at_post["attempt"]["previous_review_ids"], json!([90]));
    assert!(at_post["submitted"].is_null());
    assert_eq!(at_post["comments"], before["comments"]);
    let submitted = fixture.draft()?;
    assert_eq!(submitted["comments"], before["comments"]);
    assert_eq!(
        submitted["submitted"],
        json!({
            "id": 501, "state": "APPROVED", "url": format!("{PR_URL}#pullrequestreview-501"),
        })
    );
    assert_review_template(&fixture.editors()?[0], &before["comments"]);
    for action in ["approve", "request-changes"] {
        failure(&fixture.run(&[action], "")?);
    }
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    assert_eq!(fixture.draft()?, submitted);
    Ok(())
}

#[test]
fn request_changes_uses_the_correct_review_event() -> Result<()> {
    let fixture = Fixture::review()?;
    success(&fixture.run(&["request-changes"], "")?);
    let posts = fixture.posts()?;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["payload"]["event"], "REQUEST_CHANGES");
    assert_eq!(posts[0]["payload"]["body"], SUMMARY);
    assert_eq!(posts[0]["payload"]["commit_id"], fixture.head);
    assert_eq!(fixture.draft()?["submitted"]["state"], "CHANGES_REQUESTED");
    assert_eq!(fixture.editors()?[0]["initial"], "");
    Ok(())
}

#[test]
fn editor_override_is_split_into_arguments_without_a_shell() -> Result<()> {
    let fixture = Fixture::review()?;
    let marker = fixture.root.join("shell must not run");
    let editor = format!(
        "{} 'two words' ; touch {}",
        shell_quote(&fixture.editor.to_string_lossy()),
        shell_quote(&marker.to_string_lossy())
    );
    let mut command = fixture.cli(&["--editor", &editor, "approve"]);
    command.env("EDITOR", "nonexistent-editor-must-not-run");
    success(&fixture.execute(command, "")?);
    assert!(!marker.exists());
    let editors = fixture.editors()?;
    assert_eq!(editors.len(), 1);
    assert_eq!(
        editors[0]["args"],
        json!(["two words", ";", "touch", marker])
    );
    assert_eq!(editors[0]["initial"], "");
    assert_eq!(fixture.posts()?.len(), 1);
    Ok(())
}

#[test]
fn submit_does_not_accept_body_flags() -> Result<()> {
    let fixture = Fixture::new()?;
    for action in ["approve", "request-changes"] {
        failure(&fixture.run(&[action, "--body", "Bypass the editor"], "")?);
    }
    assert!(fixture.editors()?.is_empty());
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn a_head_or_base_change_during_editing_prevents_post() -> Result<()> {
    for mode in ["change_head", "change_base"] {
        let fixture = Fixture::review()?;
        fixture.comment(1, "one", "Keep me")?;
        let before = fixture.draft()?;
        fixture.configure("editor_mode", json!(mode))?;
        failure(&fixture.run(&["approve"], "")?);
        assert_eq!(fixture.editors()?.len(), 1);
        assert!(fixture.posts()?.is_empty());
        assert_eq!(fixture.draft()?, before);
    }
    Ok(())
}

#[test]
fn failed_post_retains_attempt_and_blocks_further_mutations() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("fail")?;
    assert_eq!(
        uncertain["attempt"]["payload"],
        fixture.posts()?[0]["payload"]
    );
    assert_eq!(
        fixture.posts()?[0]["drafts_at_post"][0]["attempt"],
        uncertain["attempt"]
    );
    for args in [
        vec!["approve"],
        vec!["request-changes"],
        vec!["comment"],
        vec!["edit", "1"],
        vec!["remove", "1"],
        vec!["discard", "--yes"],
    ] {
        failure(&fixture.run(&args, "Must not be saved")?);
        assert_eq!(fixture.draft()?, uncertain);
    }
    fixture.run(&["recover"], "")?;
    assert_eq!(
        fixture.draft()?,
        uncertain,
        "recover must not clear uncertainty by itself"
    );
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    Ok(())
}

#[test]
fn recover_finds_an_accepted_review_after_checkout_changes_without_reposting() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("lost_response")?;
    fixture.jj(&["new", "-m", "Moved away from the reviewed commit"])?;
    fixture.configure("deny_discovery", json!(true))?;
    let calls_before = fixture.calls()?.len();
    success(&fixture.saved(&["recover"], "")?);
    let recovered = fixture.draft()?;
    assert_eq!(recovered["comments"], uncertain["comments"]);
    assert_eq!(recovered["submitted"]["id"], 501);
    assert_eq!(recovered["submitted"]["state"], "APPROVED");
    assert_eq!(
        recovered["submitted"]["url"],
        format!("{PR_URL}#pullrequestreview-501")
    );
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    let calls = fixture.calls()?;
    assert!(calls[calls_before..].iter().all(|call| {
        call["method"] == "GET"
            && (call["endpoint"] == "user"
                || call["endpoint"].as_str().unwrap_or("").contains("/reviews"))
    }));
    assert!(
        calls[calls_before..]
            .iter()
            .any(|call| call["endpoint"] == "user")
    );
    assert!(calls[calls_before..].iter().any(|call| {
        call["endpoint"]
            .as_str()
            .unwrap_or("")
            .contains("/reviews/501/comments")
    }));
    Ok(())
}

#[test]
fn recover_finds_an_accepted_review_after_the_pr_closes() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("lost_response")?;
    fixture.configure("pr_state", json!("closed"))?;
    success(&fixture.saved(&["recover"], "")?);
    let recovered = fixture.draft()?;
    assert_eq!(recovered["comments"], uncertain["comments"]);
    assert_eq!(recovered["pr"], uncertain["pr"]);
    assert_eq!(recovered["submitted"]["id"], 501);
    assert_eq!(recovered["submitted"]["state"], "APPROVED");
    assert!(recovered["attempt"].is_null());
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    Ok(())
}

#[test]
fn recover_uses_original_coordinates_when_current_coordinates_are_null_or_moved() -> Result<()> {
    for (line, start_line) in [(Value::Null, Value::Null), (json!(12), json!(11))] {
        let fixture = Fixture::review()?;
        let uncertain = fixture.fail_submission("lost_response")?;
        let mut control = fixture.control()?;
        let comment = &mut control["review_comments"]["501"][0];
        comment["line"] = line;
        comment["start_line"] = start_line;
        comment["commit_id"] = json!(fixture.base);
        assert_eq!(comment["original_line"], 2);
        assert_eq!(comment["original_start_line"], 1);
        assert_eq!(comment["original_commit_id"], fixture.head);
        fixture.set_control(control)?;
        success(&fixture.saved(&["recover"], "")?);
        let recovered = fixture.draft()?;
        assert_eq!(recovered["comments"], uncertain["comments"]);
        assert_eq!(recovered["submitted"]["id"], 501);
        assert!(recovered["attempt"].is_null());
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.editors()?.len(), 1);
    }
    Ok(())
}

#[test]
fn recover_finds_a_dismissed_review_with_an_otherwise_exact_fingerprint() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("lost_response")?;
    let mut control = fixture.control()?;
    control["reviews"][0]["state"] = json!("DISMISSED");
    fixture.set_control(control)?;
    success(&fixture.saved(&["recover"], "")?);
    let recovered = fixture.draft()?;
    assert_eq!(recovered["comments"], uncertain["comments"]);
    assert_eq!(recovered["submitted"]["id"], 501);
    assert_eq!(recovered["submitted"]["state"], "DISMISSED");
    assert!(recovered["attempt"].is_null());
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    Ok(())
}

#[test]
fn saved_recovery_refuses_a_changed_authenticated_account_and_preserves_the_draft() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("lost_response")?;
    fixture.jj(&["new", "-m", "Different checkout"])?;
    fixture.configure("deny_discovery", json!(true))?;
    fixture.configure("reviewer", json!("another-account"))?;
    let calls_before = fixture.calls()?.len();
    for args in [vec!["recover"], vec!["recover", "--retry"]] {
        let output = fixture.saved(&args, "yes\n")?;
        failure(&output);
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            stderr.contains("authenticated") && stderr.contains("changed"),
            "{}",
            describe(&output)
        );
        assert_eq!(fixture.draft()?, uncertain);
    }
    let calls = fixture.calls()?;
    assert!(
        calls[calls_before..]
            .iter()
            .any(|call| call["endpoint"] == "user")
    );
    assert!(
        calls[calls_before..]
            .iter()
            .all(|call| call["method"] == "GET" && call.get("discovery_denied").is_none())
    );
    assert_eq!(fixture.posts()?.len(), 1);
    assert_eq!(fixture.editors()?.len(), 1);
    fixture.configure("reviewer", json!("reviewer"))?;
    success(&fixture.saved(&["recover"], "")?);
    assert_eq!(fixture.draft()?["submitted"]["id"], 501);
    Ok(())
}

#[test]
fn recover_requires_exact_review_identity_body_revision_and_comment_coordinates() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("lost_response")?;
    let accepted = fixture.control()?;
    for mismatch in [
        "body",
        "sha",
        "author",
        "state",
        "original_line",
        "original_start_line",
        "original_commit_id",
        "side",
        "start_side",
        "path",
        "comment_body",
        "extra_comment",
    ] {
        let mut control = accepted.clone();
        match mismatch {
            "body" => control["reviews"][0]["body"] = json!("Different summary"),
            "sha" => control["reviews"][0]["commit_id"] = json!(fixture.base),
            "author" => control["reviews"][0]["user"]["login"] = json!("another-user"),
            "state" => control["reviews"][0]["state"] = json!("CHANGES_REQUESTED"),
            "original_line" => control["review_comments"]["501"][0]["original_line"] = json!(3),
            "original_start_line" => {
                control["review_comments"]["501"][0]["original_start_line"] = json!(2)
            }
            "original_commit_id" => {
                control["review_comments"]["501"][0]["original_commit_id"] = json!(fixture.base)
            }
            "side" => control["review_comments"]["501"][0]["side"] = json!("LEFT"),
            "start_side" => control["review_comments"]["501"][0]["start_side"] = json!("LEFT"),
            "path" => control["review_comments"]["501"][0]["path"] = json!("other.txt"),
            "comment_body" => control["review_comments"]["501"][0]["body"] = json!("Other comment"),
            "extra_comment" => {
                let extra = control["review_comments"]["501"][0].clone();
                control["review_comments"]["501"]
                    .as_array_mut()
                    .unwrap()
                    .push(extra);
            }
            _ => unreachable!(),
        }
        fixture.set_control(control)?;
        fixture.saved(&["recover"], "")?;
        assert_eq!(
            fixture.draft()?,
            uncertain,
            "Recovered a nonmatching review: {mismatch}"
        );
        assert_eq!(fixture.posts()?.len(), 1);
    }
    fixture.set_control(accepted)?;
    success(&fixture.saved(&["recover"], "")?);
    assert_eq!(fixture.draft()?["submitted"]["id"], 501);
    assert_eq!(fixture.posts()?.len(), 1);
    Ok(())
}

#[test]
fn recover_does_not_adopt_an_identical_review_that_predates_the_attempt() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.configure(
        "reviews",
        json!([{
            "id": 90, "state": "APPROVED", "user": {"login": "reviewer"},
            "body": SUMMARY, "commit_id": fixture.head,
            "html_url": format!("{PR_URL}#pullrequestreview-90"),
        }]),
    )?;
    fixture.configure(
        "review_comments",
        json!({"90": [{
            "path": FILE, "body": "Keep this comment", "line": 2,
            "start_line": 1, "side": "RIGHT", "start_side": "RIGHT",
            "original_line": 2, "original_start_line": 1,
            "commit_id": fixture.head, "original_commit_id": fixture.head,
        }]}),
    )?;
    let uncertain = fixture.fail_submission("fail")?;
    assert_eq!(uncertain["attempt"]["previous_review_ids"], json!([90]));
    fixture.saved(&["recover"], "")?;
    assert_eq!(fixture.draft()?, uncertain);
    assert_eq!(fixture.posts()?.len(), 1);
    Ok(())
}

#[test]
fn recover_retry_requires_acknowledgement_and_never_posts_automatically() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("fail")?;
    fixture.saved(&["recover", "--retry"], "no\n")?;
    assert_eq!(fixture.draft()?, uncertain);
    assert_eq!(fixture.posts()?.len(), 1);
    success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
    let retriable = fixture.draft()?;
    assert!(retriable["attempt"].is_null());
    assert_eq!(retriable["retry_history"], json!([uncertain["attempt"]]));
    assert!(retriable["submitted"].is_null());
    assert_eq!(retriable["comments"], uncertain["comments"]);
    assert_eq!(fixture.posts()?.len(), 1);
    fixture.configure("post", json!("success"))?;
    success(&fixture.run(&["approve"], "")?);
    assert_eq!(
        fixture.posts()?.len(),
        2,
        "Only a new explicit submit may retry"
    );
    assert_eq!(
        fixture.posts()?[1]["drafts_at_post"][0]["retry_history"],
        retriable["retry_history"]
    );
    assert_eq!(
        fixture.draft()?["retry_history"],
        retriable["retry_history"]
    );
    Ok(())
}

#[test]
fn each_authorized_retry_appends_the_active_attempt_to_history() -> Result<()> {
    let fixture = Fixture::review()?;
    let first = fixture.fail_submission("fail")?;
    success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
    fixture.configure("editor_body", json!("A different explicit retry"))?;
    failure(&fixture.run(&["approve"], "")?);
    let second = fixture.draft()?;
    assert_eq!(second["retry_history"], json!([first["attempt"]]));
    assert_ne!(second["attempt"]["payload"], first["attempt"]["payload"]);
    success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
    let retriable = fixture.draft()?;
    assert!(retriable["attempt"].is_null());
    assert!(retriable["submitted"].is_null());
    assert_eq!(
        retriable["retry_history"],
        json!([first["attempt"], second["attempt"]])
    );
    assert_eq!(retriable["comments"], first["comments"]);
    assert_eq!(fixture.posts()?.len(), 2);
    assert_eq!(fixture.editors()?.len(), 2);
    Ok(())
}

#[test]
fn delayed_acceptance_after_retry_is_recovered_before_another_post() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("delayed_acceptance")?;
    assert_eq!(fixture.control()?["reviews"], json!([]));
    success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
    let retriable = fixture.draft()?;
    assert!(retriable["attempt"].is_null());
    assert_eq!(retriable["retry_history"], json!([uncertain["attempt"]]));
    assert_eq!(fixture.posts()?.len(), 1);
    fixture.accept_delayed_review()?;
    fixture.configure("post", json!("success"))?;
    fixture.configure("editor_body", json!("A different summary for the retry"))?;
    let output = fixture.run(&["approve"], "")?;
    let recovered = fixture.draft()?;
    assert_eq!(recovered["submitted"]["id"], 501, "{}", describe(&output));
    assert_eq!(recovered["submitted"]["state"], "APPROVED");
    assert_eq!(recovered["comments"], uncertain["comments"]);
    assert_eq!(recovered["retry_history"], retriable["retry_history"]);
    assert!(recovered["attempt"].is_null());
    assert_eq!(
        fixture.posts()?.len(),
        1,
        "A delayed acceptance must not cause a duplicate POST"
    );
    assert_eq!(fixture.control()?["reviews"].as_array().unwrap().len(), 1);
    assert!(
        fixture.editors()?.len() <= 2,
        "Recovery may finish before opening an editor"
    );
    Ok(())
}

#[test]
fn delayed_acceptance_during_the_retry_editor_is_recovered_without_another_post() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("delayed_acceptance")?;
    success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
    assert_eq!(fixture.control()?["reviews"], json!([]));
    fixture.configure("post", json!("success"))?;
    fixture.configure("editor_body", json!("A different summary for the retry"))?;
    fixture.configure("editor_mode", json!("accept_delayed_review"))?;
    let output = fixture.run(&["approve"], "")?;
    let recovered = fixture.draft()?;
    assert_eq!(recovered["submitted"]["id"], 501, "{}", describe(&output));
    assert_eq!(recovered["submitted"]["state"], "APPROVED");
    assert_eq!(recovered["comments"], uncertain["comments"]);
    assert_eq!(recovered["retry_history"], json!([uncertain["attempt"]]));
    assert!(recovered["attempt"].is_null());
    assert_eq!(
        fixture.posts()?.len(),
        1,
        "Recheck past attempts after the editor closes"
    );
    assert_eq!(fixture.editors()?.len(), 2);
    assert_eq!(fixture.control()?["reviews"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn recover_retry_refuses_a_new_nonmatching_review_by_the_same_user() -> Result<()> {
    let fixture = Fixture::review()?;
    let uncertain = fixture.fail_submission("lost_response")?;
    let mut control = fixture.control()?;
    control["reviews"][0]["body"] = json!("Another review, not our attempt");
    fixture.set_control(control)?;
    fixture.saved(&["recover", "--retry"], "yes\n")?;
    assert_eq!(fixture.draft()?, uncertain);
    assert_eq!(fixture.posts()?.len(), 1);
    Ok(())
}

#[test]
fn saved_drafts_can_be_listed_edited_removed_and_discarded_without_discovery() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "First local comment")?;
    fixture.comment(3, "three", "Second local comment")?;
    fixture.jj(&["new", "-m", "Different checkout"])?;
    fixture.configure("deny_discovery", json!(true))?;
    let calls_before = fixture.calls()?.len();
    let listed = fixture.saved(&["list"], "")?;
    success(&listed);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains("First local comment") && text.contains("Second local comment"));
    fixture.configure("editor_body", json!("Edited locally"))?;
    success(&fixture.saved(&["edit", "1"], "")?);
    assert_eq!(fixture.editors()?[0]["initial"], "First local comment");
    assert_eq!(fixture.draft()?["comments"][0]["body"], "Edited locally");
    let before_remove = fixture.draft()?;
    failure(&fixture.saved(&["remove", "99"], "")?);
    assert_eq!(fixture.draft()?, before_remove);
    success(&fixture.saved(&["remove", "1"], "")?);
    assert_eq!(fixture.draft()?["comments"].as_array().unwrap().len(), 1);
    assert_eq!(
        fixture.draft()?["comments"][0]["body"],
        "Second local comment"
    );
    let before_discard = fixture.draft()?;
    let path = fixture.draft_path()?;
    let directory = path.parent().unwrap();
    fixture.saved(&["discard"], "no\n")?;
    assert_eq!(fixture.draft()?, before_discard);
    success(&fixture.saved(&["discard", "--yes"], "")?);
    assert!(
        !path.exists(),
        "Discard must remove draft.json, not save a stale empty draft"
    );
    assert!(directory.join("lock").is_file());
    assert_eq!(archives(directory)?, vec![before_discard]);
    assert!(fixture.drafts()?.is_empty());
    fixture.assert_no_comments()?;
    assert_eq!(
        fixture.calls()?.len(),
        calls_before,
        "Local operations must not invoke gh"
    );
    Ok(())
}

#[test]
fn editing_a_comment_preserves_literal_reference_prefixes() -> Result<()> {
    let fixture = Fixture::review()?;
    let original = "Original comment\n#| This is comment content";
    let edited = "#| Updated literal prefix\n  #| Indented literal prefix";
    fixture.comment(1, "one\ntwo\n", original)?;
    fixture.configure("deny_discovery", json!(true))?;
    let calls_before = fixture.calls()?.len();
    let mut expected = fixture.draft()?;
    fixture.configure("editor_mode", json!("unchanged"))?;
    success(&fixture.saved(&["edit", "1"], "")?);
    assert_eq!(fixture.draft()?, expected);

    fixture.configure("editor_mode", json!("write"))?;
    fixture.configure("editor_body", json!(edited))?;
    success(&fixture.saved(&["edit", "1"], "")?);
    expected["comments"][0]["body"] = json!(edited);
    assert_eq!(fixture.draft()?, expected);
    let editors = fixture.editors()?;
    assert_eq!(editors.len(), 2);
    assert!(editors.iter().all(|editor| editor["initial"] == original));
    assert!(fixture.posts()?.is_empty());
    assert_eq!(fixture.calls()?.len(), calls_before);
    Ok(())
}

#[test]
fn a_new_pr_revision_can_start_a_fresh_draft_after_offline_discard() -> Result<()> {
    let mut fixture = Fixture::review()?;
    fixture.comment(1, "one", "Comment on the old revision")?;
    let before = fixture.draft()?;
    let path = fixture.draft_path()?;
    let directory = path.parent().unwrap();
    fixture.jj(&["edit", &fixture.head])?;
    fixture.jj(&["new", "-m", "New PR revision"])?;
    let new_head = fixture.sha("@")?;
    assert_ne!(new_head, fixture.head);
    fixture.configure("head", json!(new_head))?;
    fixture.jj(&["bookmark", "set", "topic", "-r", "@"])?;
    // An empty child is still a valid checkout of H1. Select the new H2 session
    // to verify that its pins cannot authorize changes to the existing H1 draft.
    fixture.review_file = Some(fixture.prepare_session()?.join("head").join(FILE));
    failure(&fixture.run(&["comment"], "Must not reuse stale comments")?);
    assert_eq!(fixture.draft()?, before);
    fixture.configure("deny_discovery", json!(true))?;
    let calls_before = fixture.calls()?.len();
    let mut discard = fixture.cli(&[]);
    discard
        .arg("--draft-dir")
        .arg(directory)
        .args(["discard", "--yes"]);
    success(&fixture.execute(discard, "")?);
    assert_eq!(fixture.calls()?.len(), calls_before);
    assert!(!path.exists());
    assert!(directory.join("lock").is_file());
    assert_eq!(archives(directory)?, vec![before.clone()]);
    fixture.configure("deny_discovery", json!(false))?;
    fixture.review_file = Some(fixture.prepare_session()?.join("head").join(FILE));
    fixture.comment(1, "one", "Comment on the new revision")?;
    assert_eq!(fixture.draft_path()?, path);
    let fresh = fixture.draft()?;
    assert_eq!(fresh["pr"]["head_sha"], new_head);
    assert_eq!(fresh["pr"]["base_sha"], fixture.base);
    assert_eq!(fresh["comments"].as_array().unwrap().len(), 1);
    assert_eq!(fresh["comments"][0]["body"], "Comment on the new revision");
    assert!(fresh["attempt"].is_null());
    assert!(fresh["submitted"].is_null());
    assert_eq!(fresh["retry_history"], json!([]));
    assert_eq!(archives(directory)?, vec![before]);
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn a_locked_draft_is_not_modified_and_is_usable_after_unlocking() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Do not lose me")?;
    let before = fixture.draft()?;
    let lock = File::options()
        .read(true)
        .write(true)
        .open(fixture.draft_path()?.parent().unwrap().join("lock"))?;
    lock.try_lock()?;
    failure(&fixture.saved(&["remove", "1"], "")?);
    assert_eq!(fixture.draft()?, before);
    drop(lock);
    success(&fixture.saved(&["list"], "")?);
    success(&fixture.saved(&["remove", "1"], "")?);
    fixture.assert_no_comments()?;
    Ok(())
}

#[test]
fn ready_marks_once_and_repeated_ready_does_not_create_review_state() -> Result<()> {
    let fixture = Fixture::new()?;
    // Unlike approving, marking your own PR ready is allowed.
    fixture.configure("author", json!("reviewer"))?;
    success(&fixture.run_ready(&[])?);
    fixture.assert_ready_mutations(&["ready"])?;
    assert_eq!(fixture.control()?["draft"], false);
    assert_eq!(
        fixture.ready_mutations()?[0]["payload"]["variables"],
        json!({"id": PR_NODE_ID})
    );
    let calls = fixture.calls()?;
    let marked = calls
        .iter()
        .position(|call| call["mutation"] == "ready")
        .unwrap();
    // Discovery, the ready baseline, and the guard each read the PR and account.
    assert_ready_checks(&calls[..marked], 3);
    assert_ready_checks(&calls[marked + 1..], 1);
    assert!(
        calls
            .iter()
            .filter(|call| call["args"][0] == "api" && call.get("mutation").is_none())
            .all(|call| {
                call["method"] == "GET"
                    && (call["endpoint"] == PR_ENDPOINT || call["endpoint"] == "user")
            })
    );
    let calls_before = calls.len();
    success(&fixture.run_ready(&[])?);
    fixture.assert_ready_mutations(&["ready"])?;
    assert_ready_checks(&fixture.calls()?[calls_before..], 2);
    assert_eq!(fixture.sha("@")?, fixture.head);
    fixture.assert_stateless_ready()
}

#[test]
fn ready_accepts_explicit_pr_url_or_number_and_repo_without_a_bookmark() -> Result<()> {
    for args in [
        vec!["--pr", PR_URL],
        vec!["--pr", "17", "--repo", "acme/widgets"],
    ] {
        let fixture = Fixture::new()?;
        fixture.jj(&["bookmark", "delete", "topic"])?;
        success(&fixture.run_ready(&args)?);
        fixture.assert_ready_mutations(&["ready"])?;
        assert!(fixture.calls()?.iter().all(|call| call["args"][0] == "api"));
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_accepts_empty_immutable_head_children_with_or_without_the_immutable_config() -> Result<()>
{
    for mode in ["automatic", "url", "number", "session"] {
        for keep_immutable in [true, false] {
            let fixture = if mode == "session" {
                Fixture::review()?
            } else {
                Fixture::new()?
            };
            let child = fixture.empty_immutable_head_child()?;
            if !keep_immutable {
                fixture.set_immutable_heads(&[])?;
            }
            if mode != "automatic" {
                fixture.jj(&["bookmark", "delete", "topic"])?;
            }
            let mut args = vec!["--reviewer", "alice"];
            match mode {
                "url" => args.extend(["--pr", PR_URL]),
                "number" => args.extend(["--pr", "17", "--repo", "acme/widgets"]),
                _ => {}
            }
            let state = guard_entries(&fixture.state)?;
            let output = fixture.run_ready(&args)?;
            success(&output);
            fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
            assert_eq!(
                fixture.ready_mutations()?[0]["payload"]["variables"],
                json!({"id": PR_NODE_ID})
            );
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.contains(&child) && text.contains(&fixture.head) && text.contains(PR_URL),
                "{text}"
            );
            assert_eq!(fixture.assert_empty_child(&fixture.head)?, child);
            assert_eq!(guard_entries(&fixture.state)?, state);
            assert!(fixture.posts()?.is_empty());
            assert!(fixture.editors()?.is_empty());
            if mode != "session" {
                fixture.assert_stateless_ready()?;
            }
        }
    }
    Ok(())
}

#[test]
fn ready_empty_immutable_head_child_keeps_the_captured_pr_when_checkout_moves() -> Result<()> {
    for mode in ["automatic", "explicit", "session"] {
        let mut cases = vec![
            ("pr_read_changes", json!({"1": {"jj_new": true}})),
            ("ready_after", json!({"jj_new": true})),
        ];
        if mode == "automatic" {
            cases.extend([
                ("repo_view_changes", json!({"jj_new": true})),
                ("pr_list_changes", json!({"jj_new": true})),
            ]);
        }
        for (hook, changes) in cases {
            let fixture = if mode == "session" {
                Fixture::review()?
            } else {
                Fixture::new()?
            };
            let captured = fixture.empty_immutable_head_child()?;
            fixture.set_immutable_heads(&[])?;
            let mut args = vec!["--reviewer", "alice"];
            if mode != "automatic" {
                fixture.jj(&["bookmark", "delete", "topic"])?;
            }
            if mode == "explicit" {
                args.extend(["--pr", PR_URL]);
            }
            fixture.configure("pr_reads", json!(0))?;
            fs::write(fixture.root.join("gh.jsonl"), "")?;
            fixture.configure(hook, changes)?;
            assert_ready_ignores_checkout_movement(&fixture, &args)
                .with_context(|| format!("{mode} from an empty child, movement at {hook}"))?;
            assert_eq!(fixture.sha("@-")?, captured);
            assert_eq!(fixture.sha("@--")?, fixture.head);
        }
    }
    Ok(())
}

#[test]
fn ready_empty_child_prefers_its_own_bookmark_and_pr_over_its_parent() -> Result<()> {
    let fixture = Fixture::new()?;
    let child = fixture.empty_immutable_head_child()?;
    fixture.jj(&["bookmark", "create", "parent-topic", "-r", &fixture.head])?;
    fixture.jj(&["bookmark", "set", "topic", "-r", &child])?;
    fixture.configure("head", json!(child))?;
    success(&fixture.run_ready(&[])?);
    fixture.assert_ready_mutations(&["ready"])?;
    let calls = fixture.calls()?;
    let listings: Vec<_> = calls
        .iter()
        .filter(|call| call["args"][0] == "pr")
        .collect();
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0]["args"][5], "topic");
    assert_eq!(fixture.sha("@")?, child);
    fixture.assert_stateless_ready()
}

#[test]
fn ready_empty_child_does_not_hide_ambiguous_bookmarks_by_using_its_parent() -> Result<()> {
    let fixture = Fixture::new()?;
    let child = fixture.empty_immutable_head_child()?;
    fixture.jj(&["bookmark", "create", "first-child", "second-child"])?;
    let output = fixture.run_ready(&[])?;
    failure(&output);
    assert!(describe(&output).to_ascii_lowercase().contains("bookmark"));
    assert!(
        fixture.calls()?.is_empty(),
        "Do not fall back to the parent's topic bookmark"
    );
    assert_eq!(fixture.sha("@")?, child);
    fixture.assert_stateless_ready()
}

#[test]
fn ready_empty_child_does_not_retry_parent_discovery_after_its_own_pr_mismatches() -> Result<()> {
    let fixture = Fixture::new()?;
    let child = fixture.empty_immutable_head_child()?;
    fixture.jj(&["bookmark", "create", "parent-topic", "-r", &fixture.head])?;
    fixture.jj(&["bookmark", "set", "topic", "-r", &child])?;
    // The listed PR still has the parent's SHA, not this bookmark's commit.
    failure(&fixture.run_ready(&[])?);
    let calls = fixture.calls()?;
    let listings: Vec<_> = calls
        .iter()
        .filter(|call| call["args"][0] == "pr")
        .collect();
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0]["args"][5], "topic");
    fixture.assert_ready_mutations(&[])?;
    assert_eq!(fixture.sha("@")?, child);
    fixture.assert_stateless_ready()
}

fn ready_checkout_movement_cases() -> Vec<(&'static str, Value)> {
    // Discovery, baseline, mark guard, mark verification, reviewer guard, and
    // reviewer verification must all keep the original revision.
    let mut cases: Vec<_> = (1..=6)
        .map(|read| {
            (
                "pr_read_changes",
                json!({(read.to_string()): {"jj_new": true}}),
            )
        })
        .collect();
    cases.extend([
        ("ready_after", json!({"jj_new": true})),
        ("request_reviewer_after", json!({"jj_new": true})),
    ]);
    cases
}

fn assert_ready_ignores_checkout_movement(fixture: &Fixture, args: &[&str]) -> Result<()> {
    let captured = fixture.sha("@")?;
    let state_before = guard_entries(&fixture.state)?;
    let output = fixture.run_ready(args)?;
    ensure!(output.status.success(), "{}", describe(&output));
    fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
    let mutations = fixture.ready_mutations()?;
    assert_eq!(
        mutations[0]["payload"]["variables"],
        json!({"id": PR_NODE_ID})
    );
    assert_eq!(mutations[1]["payload"], json!({"reviewers": ["alice"]}));

    let control = fixture.control()?;
    assert_eq!(control["draft"], false);
    assert_eq!(control["requested_reviewers"], json!(["alice"]));
    assert_eq!(control["head"], fixture.head);
    let moved = control["jj_new_head"]
        .as_str()
        .context("The checkout movement hook must run")?;
    assert_ne!(moved, fixture.head);
    assert_eq!(fixture.sha("@")?, moved, "Leave the new checkout untouched");
    assert_eq!(fixture.sha("@-")?, captured);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&captured), "{}", describe(&output));
    assert!(stdout.contains(PR_URL), "{}", describe(&output));
    assert!(stdout.contains(&fixture.head), "{}", describe(&output));
    assert!(!stdout.contains(moved), "{}", describe(&output));

    let calls = fixture.calls()?;
    let marked = calls
        .iter()
        .position(|call| call["mutation"] == "ready")
        .unwrap();
    let requested = calls
        .iter()
        .position(|call| call["mutation"] == "request-reviewer")
        .unwrap();
    assert_ready_checks(&calls[..marked], 3);
    assert_ready_checks(&calls[marked + 1..requested], 2);
    assert_ready_checks(&calls[requested + 1..], 1);
    assert_eq!(guard_entries(&fixture.state)?, state_before);
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn ready_automatic_discovery_ignores_checkout_movement_at_every_pr_read_and_write() -> Result<()> {
    for (hook, changes) in ready_checkout_movement_cases() {
        let fixture = Fixture::new()?;
        fixture.configure(hook, changes.clone())?;
        assert_ready_ignores_checkout_movement(&fixture, &["--reviewer", "alice"])
            .with_context(|| format!("Checkout movement at {hook}: {changes}"))?;
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_explicit_pr_ignores_checkout_movement_at_every_pr_read_and_write() -> Result<()> {
    for args in [
        vec!["--pr", PR_URL, "--reviewer", "alice"],
        vec![
            "--pr",
            "17",
            "--repo",
            "acme/widgets",
            "--reviewer",
            "alice",
        ],
    ] {
        for (hook, changes) in ready_checkout_movement_cases() {
            let fixture = Fixture::new()?;
            fixture.jj(&["bookmark", "delete", "topic"])?;
            fixture.configure(hook, changes.clone())?;
            assert_ready_ignores_checkout_movement(&fixture, &args)
                .with_context(|| format!("{args:?}, checkout movement at {hook}: {changes}"))?;
            assert!(fixture.calls()?.iter().all(|call| call["args"][0] == "api"));
            fixture.assert_stateless_ready()?;
        }
    }
    Ok(())
}

#[test]
fn ready_existing_session_without_a_bookmark_ignores_checkout_movement() -> Result<()> {
    for (hook, changes) in ready_checkout_movement_cases() {
        let fixture = Fixture::review()?;
        fixture.jj(&["bookmark", "delete", "topic"])?;
        // Session setup already read the PR; count only reads made by ready.
        fixture.configure("pr_reads", json!(0))?;
        fs::write(fixture.root.join("gh.jsonl"), "")?;
        fixture.configure(hook, changes.clone())?;
        assert_ready_ignores_checkout_movement(&fixture, &["--reviewer", "alice"])
            .with_context(|| format!("Session checkout movement at {hook}: {changes}"))?;
        assert!(fixture.calls()?.iter().all(|call| call["args"][0] == "api"));
    }
    Ok(())
}

#[test]
fn ready_automatic_discovery_pins_the_revision_before_repo_view_and_pr_list() -> Result<()> {
    for hook in ["repo_view_changes", "pr_list_changes"] {
        let fixture = Fixture::new()?;
        fixture.configure(hook, json!({"jj_new": true}))?;
        assert_ready_ignores_checkout_movement(&fixture, &["--reviewer", "alice"])
            .with_context(|| format!("Checkout movement at {hook}"))?;
        let calls = fixture.calls()?;
        assert_eq!(calls[0]["args"][0], "repo");
        assert_eq!(calls[0]["args"][1], "view");
        assert_eq!(calls[1]["args"][0], "pr");
        assert_eq!(calls[1]["args"][1], "list");
        assert_eq!(calls[2]["endpoint"], PR_ENDPOINT);
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_uses_the_explicit_reviewer_or_environment_and_accepts_org_teams() -> Result<()> {
    for (environment, explicit, expected) in [
        (None, Some("alice"), Some("alice")),
        (Some("env-user"), None, Some("env-user")),
        (
            Some("invalid;environment"),
            Some("acme/review-team"),
            Some("acme/review-team"),
        ),
        (Some(" \t\n "), None, None),
    ] {
        let fixture = Fixture::new()?;
        let mut command = fixture.ready_cli(&[]);
        if let Some(value) = environment {
            command.env("ZED_REVISION_READY_REVIEWER", value);
        }
        if let Some(value) = explicit {
            command.args(["--reviewer", value]);
        }
        success(&fixture.execute(command, "")?);
        if let Some(value) = expected {
            fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
            assert_eq!(fixture.control()?["requested_reviewers"], json!([value]));
            let mutations = fixture.ready_mutations()?;
            assert_eq!(mutations[1]["reviewer"], value);
            let payload = if let Some((_, team)) = value.split_once('/') {
                json!({"team_reviewers": [team]})
            } else {
                json!({"reviewers": [value]})
            };
            assert_eq!(mutations[1]["payload"], payload);
            let calls = fixture.calls()?;
            let marked = calls
                .iter()
                .position(|call| call["mutation"] == "ready")
                .unwrap();
            let requested = calls
                .iter()
                .position(|call| call["mutation"] == "request-reviewer")
                .unwrap();
            assert_ready_checks(&calls[..marked], 3);
            assert_ready_checks(&calls[marked + 1..requested], 1);
            assert_ready_checks(&calls[requested + 1..], 1);
        } else {
            fixture.assert_ready_mutations(&["ready"])?;
            assert!(fixture.control()?.get("requested_reviewers").is_none());
        }
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_requests_a_reviewer_on_an_already_ready_pr_without_marking_again() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure("draft", json!(false))?;
    success(&fixture.run_ready(&["--reviewer", "acme/review-team"])?);
    fixture.assert_ready_mutations(&["request-reviewer"])?;
    assert_eq!(
        fixture.control()?["requested_reviewers"],
        json!(["acme/review-team"])
    );
    let calls = fixture.calls()?;
    let requested = calls
        .iter()
        .position(|call| call["mutation"] == "request-reviewer")
        .unwrap();
    assert_ready_checks(&calls[..requested], 3);
    assert_ready_checks(&calls[requested + 1..], 1);
    fixture.assert_stateless_ready()
}

#[test]
fn ready_rejects_invalid_reviewers_before_any_mutation() -> Result<()> {
    let fixture = Fixture::new()?;
    for value in [
        "",
        " \t",
        "--admin",
        "alice,bob",
        "alice;touch injected-ready",
        "alice$(id)",
        "acme/team/extra",
    ] {
        let mut command = fixture.ready_cli(&[]);
        // An equals sign passes even a flag-shaped value through to validation.
        command.arg(format!("--reviewer={value}"));
        command.env("ZED_REVISION_READY_REVIEWER", "valid-fallback");
        let output = fixture.execute(command, "")?;
        failure(&output);
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("reviewer"),
            "{}",
            describe(&output)
        );
    }
    let mut command = fixture.ready_cli(&[]);
    command.env("ZED_REVISION_READY_REVIEWER", "alice,bob");
    failure(&fixture.execute(command, "")?);
    fixture.assert_ready_mutations(&[])?;
    assert!(!fixture.repo.join("injected-ready").exists());
    fixture.assert_stateless_ready()
}

#[test]
fn ready_rejects_a_pr_head_that_differs_from_jj_at() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure("head", json!(fixture.base))?;
    let output = fixture.run_ready(&["--pr", PR_URL, "--reviewer", "alice"])?;
    failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("jj @"),
        "{}",
        describe(&output)
    );
    fixture.assert_ready_mutations(&[])?;
    assert_eq!(fixture.sha("@")?, fixture.head);
    fixture.assert_stateless_ready()
}

#[test]
fn ready_never_falls_back_from_nonempty_work_even_with_an_explicit_pr() -> Result<()> {
    let fixture = Fixture::new()?;
    let child = fixture.nonempty_child("Unbookmarked nonempty child of the PR head")?;
    assert_ne!(child, fixture.head);
    assert_eq!(fixture.sha("@-")?, fixture.head);
    failure(&fixture.run_ready(&[])?);
    assert!(
        fixture.calls()?.is_empty(),
        "No bookmark at @ must stop discovery before gh"
    );
    for args in [
        vec!["--pr", PR_URL],
        vec!["--pr", "17", "--repo", "github.com/acme/widgets"],
    ] {
        let output = fixture.run_ready(&args)?;
        failure(&output);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("jj @"),
            "{}",
            describe(&output)
        );
    }
    fixture.assert_ready_mutations(&[])?;
    assert_eq!(fixture.sha("@")?, child);
    fixture.assert_stateless_ready()
}

#[test]
fn ready_existing_session_never_authorizes_an_initial_checkout_at_a_nonempty_child() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.jj(&["bookmark", "delete", "topic"])?;
    let child = fixture.nonempty_child("Unbookmarked nonempty child of the session head")?;
    let state_before = guard_entries(&fixture.state)?;
    let output = fixture.run_ready(&["--reviewer", "alice"])?;
    failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("jj @"),
        "{}",
        describe(&output)
    );
    fixture.assert_ready_mutations(&[])?;
    assert_eq!(fixture.sha("@")?, child);
    assert_eq!(fixture.sha("@-")?, fixture.head);
    assert_eq!(guard_entries(&fixture.state)?, state_before);
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn ready_bypasses_a_locked_uncertain_review_draft_without_changing_it() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.fail_submission("lost_response")?;
    let path = fixture.draft_path()?;
    let before = fs::read(&path)?;
    let directory = path.parent().unwrap();
    let lock = File::options()
        .read(true)
        .write(true)
        .open(directory.join("lock"))?;
    lock.try_lock()?;
    let editors_before = fixture.editors()?.len();
    let posts_before = fixture.posts()?.len();
    success(&fixture.run_ready(&["--reviewer", "alice"])?);
    fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
    assert_eq!(fs::read(&path)?, before);
    assert_eq!(fixture.drafts()?, vec![path.clone()]);
    assert!(archives(directory)?.is_empty());
    assert_eq!(fixture.editors()?.len(), editors_before);
    assert_eq!(fixture.posts()?.len(), posts_before);
    drop(lock);
    Ok(())
}

#[test]
fn ready_does_not_open_or_validate_the_state_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    // A regular file cannot hold a Store, but ready must ignore it entirely.
    fs::write(&fixture.state, "not a directory")?;
    success(&fixture.run_ready(&[])?);
    fixture.assert_ready_mutations(&["ready"])?;
    assert_eq!(fs::read_to_string(&fixture.state)?, "not a directory");
    assert!(fixture.posts()?.is_empty());
    assert!(fixture.editors()?.is_empty());
    Ok(())
}

#[test]
fn ready_rejects_draft_dir_before_discovery() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut command = fixture.ready_cli(&["--reviewer", "alice"]);
    command
        .arg("--draft-dir")
        .arg(fixture.state.join("must-not-open"));
    let output = fixture.execute(command, "")?;
    failure(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--draft-dir"),
        "{}",
        describe(&output)
    );
    assert!(fixture.calls()?.is_empty());
    fixture.assert_stateless_ready()
}

#[test]
fn ready_failure_never_retries_or_requests_a_reviewer() -> Result<()> {
    for mode in ["fail", "lost_response"] {
        let fixture = Fixture::new()?;
        fixture.configure("ready", json!(mode))?;
        let output = fixture.run_ready(&["--reviewer", "alice"])?;
        failure(&output);
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            error.contains("ready")
                && error.contains("reviewer")
                && (error.contains("not") || error.contains("skipp")),
            "{}",
            describe(&output)
        );
        if mode == "lost_response" {
            assert!(
                error.contains("uncertain")
                    || error.contains("may have")
                    || error.contains("might have"),
                "{}",
                describe(&output)
            );
        }
        fixture.assert_ready_mutations(&["ready"])?;
        assert_eq!(
            fixture.control()?["draft"].as_bool().unwrap_or(true),
            mode == "fail"
        );
        assert!(fixture.control()?.get("requested_reviewers").is_none());
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_reviewer_failure_reports_partial_success_without_retrying() -> Result<()> {
    for mode in ["fail", "lost_response"] {
        let fixture = Fixture::new()?;
        fixture.configure("request_reviewer", json!(mode))?;
        let output = fixture.run_ready(&["--reviewer", "alice"])?;
        failure(&output);
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(error.contains("reviewer"), "{}", describe(&output));
        assert!(
            [
                "is ready",
                "already ready",
                "marked ready",
                "now ready",
                "remains ready",
                "ready for review",
                "pr ready"
            ]
            .iter()
            .any(|&phrase| error.contains(phrase)),
            "Must report that the PR is ready: {}",
            describe(&output)
        );
        if mode == "lost_response" {
            assert!(
                error.contains("uncertain")
                    || error.contains("may have")
                    || error.contains("might have"),
                "{}",
                describe(&output)
            );
        }
        fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
        assert_eq!(fixture.control()?["draft"], false);
        let requested = fixture
            .control()?
            .get("requested_reviewers")
            .cloned()
            .unwrap_or(json!([]));
        assert_eq!(
            requested,
            if mode == "fail" {
                json!([])
            } else {
                json!(["alice"])
            }
        );
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_rechecks_pr_and_account_before_marking() -> Result<()> {
    for field in ["head", "base", "reviewer", "pr_state"] {
        let fixture = Fixture::new()?;
        let changed = match field {
            "head" => json!(fixture.base),
            "base" => json!(fixture.head),
            "reviewer" => json!("different-account"),
            "pr_state" => json!("closed"),
            _ => unreachable!(),
        };
        // GET #1 belongs to discover; GET #2 is the first ready-specific read.
        fixture.configure("pr_read_changes", json!({"2": {(field): changed}}))?;
        failure(&fixture.run_ready(&["--pr", PR_URL, "--reviewer", "alice"])?);
        assert!(fixture.control()?["pr_reads"].as_u64().unwrap() >= 2);
        fixture.assert_ready_mutations(&[])?;
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_detects_changes_after_marking_and_stops_before_requesting_a_reviewer() -> Result<()> {
    for field in [
        "head", "base", "base_ref", "node_id", "reviewer", "pr_state", "draft",
    ] {
        let fixture = Fixture::new()?;
        let changed = match field {
            "head" => json!(fixture.base),
            "base" => json!(fixture.head),
            "reviewer" => json!("different-account"),
            "pr_state" => json!("closed"),
            "base_ref" => json!("release"),
            "node_id" => json!("PR_replaced17"),
            "draft" => json!(true),
            _ => unreachable!(),
        };
        fixture.configure("ready_after", json!({(field): changed}))?;
        let output = fixture.run_ready(&["--reviewer", "alice"])?;
        failure(&output);
        fixture.assert_ready_mutations(&["ready"])?;
        if matches!(field, "base_ref" | "node_id") {
            assert_eq!(fixture.control()?["base"], fixture.base);
            assert_eq!(fixture.control()?["head"], fixture.head);
            let calls = fixture.calls()?;
            let marked = calls
                .iter()
                .position(|call| call["mutation"] == "ready")
                .unwrap();
            assert_ready_checks(&calls[marked + 1..], 1);
        }
        assert!(fixture.control()?.get("requested_reviewers").is_none());
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_rechecks_after_requesting_a_reviewer_even_if_the_response_is_lost() -> Result<()> {
    for mode in ["success", "lost_response"] {
        let fixture = Fixture::new()?;
        fixture.configure("request_reviewer", json!(mode))?;
        fixture.configure("request_reviewer_after", json!({"head": fixture.base}))?;
        let output = fixture.run_ready(&["--reviewer", "alice"])?;
        failure(&output);
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(error.contains("head"), "{}", describe(&output));
        fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
        assert_eq!(fixture.control()?["draft"], false);
        assert_eq!(fixture.control()?["requested_reviewers"], json!(["alice"]));
        let calls = fixture.calls()?;
        let requested = calls
            .iter()
            .position(|call| call["mutation"] == "request-reviewer")
            .unwrap();
        assert_ready_checks(&calls[requested + 1..], 1);
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_requires_a_boolean_draft_field_without_changing_discovery() -> Result<()> {
    for patch in [
        json!({"omit_draft": true}),
        json!({"draft": "false"}),
        json!({"draft": null}),
    ] {
        let fixture = Fixture::new()?;
        for (key, value) in patch.as_object().unwrap() {
            fixture.configure(key, value.clone())?;
        }
        let output = fixture.run_ready(&["--pr", PR_URL])?;
        failure(&output);
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("draft"),
            "{}",
            describe(&output)
        );
        assert!(
            fixture.control()?["pr_reads"].as_u64().unwrap() >= 2,
            "Discovery must still accept PR JSON without a boolean draft field"
        );
        fixture.assert_ready_mutations(&[])?;
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_preserves_an_explicit_hostname_for_both_api_mutations() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure("host", json!("www.github.com"))?;
    fixture.configure("node_id", json!("PR_www17"))?;
    success(&fixture.run_ready(&[
        "--pr",
        "https://www.github.com/acme/widgets/pull/17",
        "--reviewer",
        "alice",
    ])?);
    fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
    assert_eq!(
        fixture.ready_mutations()?[0]["payload"]["variables"],
        json!({"id": "PR_www17"})
    );
    assert_eq!(
        fixture.ready_mutations()?[1]["payload"],
        json!({"reviewers": ["alice"]})
    );
    for call in fixture.calls()? {
        assert_eq!(call["args"][0], "api");
        assert_eq!(call["hostname"], "www.github.com");
    }
    fixture.assert_stateless_ready()
}

#[test]
fn ready_adds_user_or_team_requests_without_replacing_existing_reviewers() -> Result<()> {
    for reviewer in ["alice", "acme/review-team"] {
        let fixture = Fixture::new()?;
        fixture.configure(
            "requested_reviewers",
            json!(["existing-user", "acme/existing-team"]),
        )?;
        // Another request arrives after marking, before our reviewer mutation.
        fixture.configure(
            "ready_after",
            json!({
                "requested_reviewers": ["existing-user", "acme/existing-team", "concurrent-user"],
            }),
        )?;
        success(&fixture.run_ready(&["--reviewer", reviewer])?);
        fixture.assert_ready_mutations(&["ready", "request-reviewer"])?;
        assert_eq!(
            fixture.control()?["requested_reviewers"],
            json!([
                "existing-user",
                "acme/existing-team",
                "concurrent-user",
                reviewer,
            ])
        );
        let payload = if reviewer == "alice" {
            json!({"reviewers": ["alice"]})
        } else {
            json!({"team_reviewers": ["review-team"]})
        };
        assert_eq!(fixture.ready_mutations()?[1]["payload"], payload);
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

#[test]
fn ready_rejects_a_team_from_another_org_before_marking() -> Result<()> {
    let fixture = Fixture::new()?;
    failure(&fixture.run_ready(&["--reviewer", "other-org/review-team"])?);
    fixture.assert_ready_mutations(&[])?;
    assert!(fixture.control()?.get("requested_reviewers").is_none());
    fixture.assert_stateless_ready()
}

#[test]
fn ready_requires_a_nonempty_node_id_without_changing_discovery() -> Result<()> {
    for patch in [json!({"omit_node_id": true}), json!({"node_id": ""})] {
        let fixture = Fixture::new()?;
        for (key, value) in patch.as_object().unwrap() {
            fixture.configure(key, value.clone())?;
        }
        let output = fixture.run_ready(&["--pr", PR_URL, "--reviewer", "alice"])?;
        failure(&output);
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("node"),
            "{}",
            describe(&output)
        );
        assert!(
            fixture.control()?["pr_reads"].as_u64().unwrap() >= 2,
            "Discovery must still accept PR JSON without a nonempty node_id"
        );
        fixture.assert_ready_mutations(&[])?;
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}

// Finish/cancel/submit-comment regressions use only the public CLI after setup.
fn review_flow_command(fixture: &Fixture, directory: &Path, side: &str, args: &[&str]) -> Command {
    navigation_command(fixture, args, Some(&directory.join(side).join(FILE)))
}

fn review_flow_assert_cancelled(session: &CleanupSession) -> Result<()> {
    let files = cleanup_contents(&session.directory)?;
    assert_eq!(
        files.keys().cloned().collect::<Vec<_>>(),
        [
            PathBuf::from("cancelled.json"),
            PathBuf::from("manifest.json")
        ]
    );
    assert_eq!(
        files[Path::new("manifest.json")],
        session.files[Path::new("manifest.json")]
    );
    assert!(!session.directory.join("head").exists());
    assert!(!session.directory.join("base").exists());
    Ok(())
}

struct ReviewFlowCheckout {
    head: String,
    bookmarks: String,
    refs: String,
    work: Vec<u8>,
}

impl ReviewFlowCheckout {
    fn capture(fixture: &Fixture) -> Result<Self> {
        Ok(Self {
            head: fixture.sha("@")?,
            bookmarks: fixture.jj(&[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "--all-remotes",
            ])?,
            refs: fixture.git(&["for-each-ref", "--format=%(refname) %(objectname)"])?,
            work: fs::read(fixture.repo.join(FILE))?,
        })
    }

    fn assert_kept(&self, fixture: &Fixture) -> Result<()> {
        let after = Self::capture(fixture)?;
        assert_eq!(
            after.head, self.head,
            "Review action moved or snapshotted the checkout"
        );
        assert_eq!(after.bookmarks, self.bookmarks);
        assert_eq!(after.refs, self.refs);
        assert_eq!(after.work, self.work);
        for call in navigation_calls(fixture)? {
            let args = call["args"].as_array().unwrap();
            for forbidden in [
                "edit", "new", "describe", "abandon", "restore", "fetch", "push", "sb", "sf",
            ] {
                assert!(
                    !args.iter().any(|arg| arg == forbidden),
                    "Unexpected mutation: {call}"
                );
            }
        }
        Ok(())
    }
}

#[test]
fn review_flow_menu_abandons_empty_or_commented_drafts_from_either_side_offline() -> Result<()> {
    for side in ["head", "base"] {
        for commented in [false, true] {
            let fixture = navigation_fixture()?;
            let first = CleanupSession::capture(&fixture.prepare_session()?)?;
            let latest = CleanupSession::capture(&fixture.prepare_session()?)?;
            if commented {
                success(&fixture.run_session(
                    &latest.directory,
                    &["comment"],
                    "Private draft\nKeep in the archive",
                )?);
            }
            let before = fixture
                .drafts()?
                .first()
                .map(|path| read_json(path))
                .transpose()?;
            // No discovery or implicit jj snapshot is permissible, even with stale
            // remote metadata and unsaved source edits outside the frozen diff.
            fixture.configure("head", json!(fixture.base))?;
            fixture.configure("deny_discovery", json!(true))?;
            fs::write(
                fixture.repo.join(FILE),
                "Unsaved source edits must survive\n",
            )?;
            let checkout = ReviewFlowCheckout::capture(&fixture)?;
            let control = fixture.control()?;
            navigation_clear_calls(&fixture)?;
            let output = fixture.execute(
                review_flow_command(&fixture, &first.directory, side, &["finish"]),
                "1\n",
            )?;
            success(&output);
            assert!(describe(&output).contains("Nothing was submitted to GitHub"));
            for session in [&first, &latest] {
                review_flow_assert_cancelled(session)?;
            }
            assert!(fixture.drafts()?.is_empty());
            assert!(!fixture.state.join("active.json").exists());
            let archived = archives(first.directory.parent().unwrap())?;
            assert_eq!(archived.len(), 1);
            if let Some(before) = before {
                assert_eq!(archived[0], before);
            } else {
                assert_eq!(archived[0]["comments"], json!([]));
                assert_eq!(archived[0]["pr"]["head_sha"], fixture.head);
                assert!(archived[0]["attempt"].is_null());
                assert!(archived[0]["submitted"].is_null());
            }
            assert!(fixture.calls()?.is_empty());
            assert!(fixture.editors()?.is_empty());
            assert!(fixture.zed_calls()?.is_empty());
            assert_eq!(fixture.control()?, control);
            checkout.assert_kept(&fixture)?;
        }
    }
    Ok(())
}

#[test]
fn review_flow_stale_draft_in_new_diff_refuses_submission_but_cancels_locally() -> Result<()> {
    for (side, args, input) in [
        ("head", vec!["finish"], "1\n"),
        ("base", vec!["cancel", "--yes"], ""),
    ] {
        let (fixture, directory) = navigation_session()?;
        let old = CleanupSession::capture(&directory)?;
        let draft = fixture.draft()?;
        let draft_bytes = fs::read(fixture.draft_path()?)?;
        fixture.jj(&["new", "-m", "H2 of the same PR"])?;
        fs::write(fixture.repo.join(FILE), "one\ntwo\nFOUR\n")?;
        fixture.jj(&["describe", "-m", "H2 of the same PR"])?;
        let h2 = fixture.sha("@")?;
        assert_ne!(h2, fixture.head);
        fixture.configure("head", json!(h2))?;
        fixture.configure(
            "files",
            json!([{
                "filename": FILE, "status": "modified",
                "patch": "@@ -1,3 +1,3 @@\n one\n two\n-three\n+FOUR"
            }]),
        )?;
        let latest = CleanupSession::capture(&fixture.prepare_session()?)?;
        assert_eq!(old.directory.parent(), latest.directory.parent());
        assert_eq!(
            read_json(&latest.directory.join("manifest.json"))?["pr"]["head_sha"],
            h2
        );
        assert_eq!(draft["pr"]["head_sha"], fixture.head);
        assert_eq!(fs::read(fixture.draft_path()?)?, draft_bytes);
        let before = NavigationCheckpoint::capture(&fixture)?;
        let checkout = ReviewFlowCheckout::capture(&fixture)?;
        navigation_clear_calls(&fixture)?;
        for choice in ["2\n", "3\n", "4\n"] {
            let output = fixture.execute(
                review_flow_command(&fixture, &latest.directory, side, &["finish"]),
                choice,
            )?;
            failure(&output);
            assert!(
                describe(&output).contains("1 pending comment"),
                "{}",
                describe(&output)
            );
            assert!(
                describe(&output).contains("PR head, base, or identity has changed"),
                "{}",
                describe(&output)
            );
            before.assert_unchanged(&fixture)?;
            old.assert_kept()?;
            latest.assert_kept()?;
            assert!(archives(directory.parent().unwrap())?.is_empty());
        }
        // Cancellation must use the saved H1 draft, not replace it with an empty
        // H2 draft or require the remote revision to match that older draft.
        fixture.configure("deny_discovery", json!(true))?;
        let control = fixture.control()?;
        navigation_clear_calls(&fixture)?;
        success(&fixture.execute(
            review_flow_command(&fixture, &latest.directory, side, &args),
            input,
        )?);
        review_flow_assert_cancelled(&old)?;
        review_flow_assert_cancelled(&latest)?;
        assert_eq!(archives(directory.parent().unwrap())?, vec![draft]);
        assert!(fixture.drafts()?.is_empty());
        assert!(!fixture.state.join("active.json").exists());
        assert!(fixture.calls()?.is_empty());
        assert!(fixture.editors()?.is_empty());
        assert_eq!(fixture.control()?, control);
        checkout.assert_kept(&fixture)?;
    }
    Ok(())
}

#[test]
fn review_flow_plain_menu_reads_one_choice_and_keeps_state_on_dismissal_or_invalid_input()
-> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    let files = cleanup_contents(&fixture.state)?;
    let checkout = ReviewFlowCheckout::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    for (input, valid) in [
        ("", true),
        ("\n1\n", true),
        (" \t\n2\n", true),
        ("q\n1\n", true),
        ("\u{1b}\n", true),
        ("\u{3}\n", true),
        ("0\n1\n", false),
        ("5\n", false),
        ("approve\n3\n", false),
        ("1 2\n", false),
    ] {
        let output = fixture.execute(
            review_flow_command(&fixture, &directory, "base", &["finish"]),
            input,
        )?;
        if valid {
            success(&output);
        } else {
            failure(&output);
        }
        for choice in [
            "1  Exit + abandon",
            "2  Comment",
            "3  Approve",
            "4  Request changes",
            "1 pending comment",
        ] {
            assert!(
                describe(&output).contains(choice),
                "Missing {choice}: {}",
                describe(&output)
            );
        }
        before.assert_unchanged(&fixture)?;
        assert_eq!(cleanup_contents(&fixture.state)?, files);
        assert!(fixture.calls()?.is_empty());
    }
    checkout.assert_kept(&fixture)?;
    Ok(())
}

#[test]
fn review_flow_cancel_confirmation_and_yes_work_after_checkout_and_remote_drift() -> Result<()> {
    for yes_flag in [false, true] {
        let (fixture, directory) = navigation_session()?;
        let session = CleanupSession::capture(&directory)?;
        let draft = fixture.draft()?;
        fixture.nonempty_child("Unrelated local work")?;
        fixture.configure("head", json!(fixture.base))?;
        fixture.configure("pr_state", json!("closed"))?;
        fixture.configure("deny_discovery", json!(true))?;
        let before = NavigationCheckpoint::capture(&fixture)?;
        let checkout = ReviewFlowCheckout::capture(&fixture)?;
        navigation_clear_calls(&fixture)?;
        for input in ["", "\n", "no\nyes\n", "y\n"] {
            success(&fixture.execute(
                review_flow_command(&fixture, &directory, "head", &["cancel"]),
                input,
            )?);
            before.assert_unchanged(&fixture)?;
            session.assert_kept()?;
            assert!(archives(directory.parent().unwrap())?.is_empty());
        }
        let args = if yes_flag {
            vec!["cancel", "--yes"]
        } else {
            vec!["cancel"]
        };
        let input = if yes_flag { "no\n" } else { "yes\n" };
        success(&fixture.execute(
            review_flow_command(&fixture, &directory, "base", &args),
            input,
        )?);
        review_flow_assert_cancelled(&session)?;
        assert_eq!(archives(directory.parent().unwrap())?, vec![draft]);
        assert!(fixture.drafts()?.is_empty());
        assert!(!fixture.state.join("active.json").exists());
        assert!(fixture.calls()?.is_empty());
        checkout.assert_kept(&fixture)?;
    }
    Ok(())
}

#[test]
fn review_flow_context_guards_refuse_before_input_network_or_state_writes() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.use_default_state();
    for active in [false, true] {
        if active {
            fixture.prepare_session()?;
        }
        for file in [None, Some(PathBuf::new()), Some(fixture.repo.join(FILE))] {
            for action in ["finish", "cancel", "submit-comment"] {
                assert_review_guard_refusal(
                    &fixture,
                    navigation_command(&fixture, &[action], file.as_deref()),
                )?;
            }
        }
        if !active {
            assert!(!fixture.state.exists());
        }
    }
    let directory = fixture.active_directory()?;
    for action in ["finish", "cancel"] {
        for args in [
            vec!["--pr", PR_URL],
            vec!["--repo", "acme/widgets"],
            vec!["--draft-dir", directory.parent().unwrap().to_str().unwrap()],
        ] {
            let mut command = review_flow_command(&fixture, &directory, "head", &[action]);
            command.args(args);
            assert_review_guard_refusal(&fixture, command)?;
        }
    }
    for action in ["finish", "cancel", "submit-comment"] {
        for file in [
            directory.join("manifest.json"),
            directory.join("head/missing.txt"),
            directory.join("head/../head").join(FILE),
        ] {
            assert_review_guard_refusal(
                &fixture,
                navigation_command(&fixture, &[action], Some(&file)),
            )?;
        }
        for environment in [false, true] {
            let external = fixture.root.join("external state");
            let mut command = review_flow_command(&fixture, &directory, "head", &[action]);
            if environment {
                command.env("ZED_PR_REVIEW_STATE_DIR", &external);
            } else {
                command.arg("--state-dir").arg(&external);
            }
            assert_review_guard_refusal(&fixture, command)?;
            assert!(!external.exists());
        }
    }
    Ok(())
}

#[test]
fn review_flow_cancel_refuses_uncertain_attempt_retry_history_and_submitted_receipt() -> Result<()>
{
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Keep until recovery is certain")?;
    let directory = fixture.active_directory()?;
    let session = CleanupSession::capture(&directory)?;
    fixture.configure("post", json!("fail"))?;
    failure(&fixture.run(&["submit-comment"], "")?);
    for retriable in [false, true] {
        if retriable {
            success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
            assert!(fixture.draft()?["attempt"].is_null());
            assert_eq!(
                fixture.draft()?["retry_history"].as_array().unwrap().len(),
                1
            );
        } else {
            assert!(!fixture.draft()?["attempt"].is_null());
        }
        let before = cleanup_contents(&fixture.state)?;
        let calls = fixture.calls()?;
        let editors = fixture.editors()?;
        for (args, input) in [
            (vec!["cancel", "--yes"], ""),
            (vec!["cancel"], "yes\n"),
            (vec!["finish"], "1\n"),
        ] {
            let output = fixture.execute(
                review_flow_command(&fixture, &directory, "head", &args),
                input,
            )?;
            failure(&output);
            assert!(describe(&output).contains("recover"));
            assert_eq!(cleanup_contents(&fixture.state)?, before);
        }
        session.assert_kept()?;
        assert_eq!(fixture.calls()?, calls);
        assert_eq!(fixture.editors()?, editors);
    }
    fixture.configure("post", json!("success"))?;
    success(&fixture.run(&["submit-comment"], "")?);
    let before = cleanup_contents(&fixture.state)?;
    let calls = fixture.calls()?;
    for args in [vec!["cancel", "--yes"], vec!["finish"]] {
        failure(&fixture.execute(
            review_flow_command(&fixture, &directory, "head", &args),
            "1\n",
        )?);
        assert_eq!(cleanup_contents(&fixture.state)?, before);
        assert_eq!(fixture.calls()?, calls);
    }
    assert_eq!(fixture.draft()?["submitted"]["state"], "COMMENTED");
    Ok(())
}

#[test]
fn review_flow_cancelled_tab_can_retry_cleanup_but_cannot_touch_a_new_diff() -> Result<()> {
    let fixture = Fixture::review()?;
    fixture.comment(1, "one", "Archive only this old comment")?;
    let old = fixture.active_directory()?;
    success(&fixture.execute(
        review_flow_command(&fixture, &old, "head", &["cancel", "--yes"]),
        "",
    )?);
    let cancelled = cleanup_contents(&fixture.state)?;
    let calls = fixture.calls()?;
    for args in [vec!["cancel", "--yes"], vec!["finish"]] {
        success(&fixture.execute(review_flow_command(&fixture, &old, "head", &args), "1\n")?);
        assert_eq!(cleanup_contents(&fixture.state)?, cancelled);
        assert_eq!(fixture.calls()?, calls);
    }
    for action in ["comment", "submit-comment", "approve", "request-changes"] {
        assert_review_guard_refusal(
            &fixture,
            review_flow_command(&fixture, &old, "head", &[action]),
        )?;
    }
    success(&fixture.run_default(&["diff", "--pr", PR_URL], "")?);
    let fresh = fixture.active_directory()?;
    assert_ne!(fresh, old);
    success(&fixture.run_session(&fresh, &["comment"], "New draft belongs to the new diff")?);
    let before = cleanup_contents(&fixture.state)?;
    for action in [
        "finish",
        "cancel",
        "comment",
        "submit-comment",
        "approve",
        "request-changes",
    ] {
        assert_review_guard_refusal(
            &fixture,
            review_flow_command(&fixture, &old, "head", &[action]),
        )?;
    }
    assert_eq!(cleanup_contents(&fixture.state)?, before);
    assert_eq!(
        fixture.draft()?["comments"][0]["body"],
        "New draft belongs to the new diff"
    );
    assert_eq!(archives(old.parent().unwrap())?.len(), 1);
    assert!(fixture.posts()?.is_empty());
    Ok(())
}

#[test]
fn review_flow_cancelling_an_old_pr_keeps_the_other_active_pr_and_its_draft() -> Result<()> {
    let (fixture, old) = navigation_session()?;
    let old_session = CleanupSession::capture(&old)?;
    let old_draft = fixture.draft()?;
    fixture.configure("additional_pr_numbers", json!([18]))?;
    let mut prepare = fixture.command(std::env::current_exe()?);
    prepare
        .args(["--exact", "session_fixture_child", "--nocapture"])
        .env("ZPR_PREPARE_SESSION", "1")
        .env(
            "ZPR_PREPARE_SESSION_PR",
            "https://github.com/acme/widgets/pull/18",
        );
    success(&fixture.execute(prepare, "")?);
    let other = fixture.active_directory()?;
    success(&fixture.run_session(&other, &["comment"], "Other PR private draft")?);
    let other_files = cleanup_contents(other.parent().unwrap())?;
    let active = fs::read(fixture.state.join("active.json"))?;
    navigation_clear_calls(&fixture)?;
    success(&fixture.execute(
        review_flow_command(&fixture, &old, "base", &["finish"]),
        "1\n",
    )?);
    review_flow_assert_cancelled(&old_session)?;
    assert_eq!(archives(old.parent().unwrap())?, vec![old_draft]);
    assert_eq!(cleanup_contents(other.parent().unwrap())?, other_files);
    assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
    assert!(fixture.calls()?.is_empty());
    Ok(())
}

fn review_flow_assert_submission(
    fixture: &Fixture,
    session: &CleanupSession,
    event: &str,
    state: &str,
    commented: bool,
) -> Result<()> {
    let posts = fixture.posts()?;
    assert_eq!(posts.len(), 1);
    let comments = if commented {
        json!([
            {"path": FILE, "start_line": 1, "line": 2, "start_side": "RIGHT", "side": "RIGHT", "body": "First inline comment\n#| Literal text"},
            {"path": FILE, "line": 3, "side": "RIGHT", "body": "Second inline comment"},
        ])
    } else {
        json!([])
    };
    let payload =
        json!({"event": event, "commit_id": fixture.head, "body": SUMMARY, "comments": comments});
    assert_eq!(posts[0]["payload"], payload);
    let at_post = &posts[0]["drafts_at_post"][0];
    assert_eq!(at_post["attempt"]["payload"], payload);
    assert!(at_post["submitted"].is_null());
    assert_eq!(
        posts[0]["sessions_at_post"][session.directory.to_str().unwrap()],
        json!({"head": true, "base": true, "cleanup.json": false})
    );
    let receipt = cleanup_assert_receipt(&fixture.draft_path()?, at_post, state)?;
    session.assert_cleaned(&receipt)?;
    assert_eq!(fixture.editors()?.len(), 1);
    if commented {
        assert_review_template(&fixture.editors()?[0], &at_post["comments"]);
    } else {
        assert_eq!(fixture.editors()?[0]["initial"], "");
    }
    assert!(archives(session.directory.parent().unwrap())?.is_empty());
    Ok(())
}

#[test]
fn review_flow_menu_submit_choices_and_submit_comment_use_editor_payload_receipt_and_cleanup()
-> Result<()> {
    for commented in [false, true] {
        for (action, input, event, state) in [
            ("finish", "2\nignored stdin", "COMMENT", "COMMENTED"),
            ("finish", "3\nignored stdin", "APPROVE", "APPROVED"),
            (
                "finish",
                "4\nignored stdin",
                "REQUEST_CHANGES",
                "CHANGES_REQUESTED",
            ),
            ("submit-comment", "ignored stdin", "COMMENT", "COMMENTED"),
        ] {
            let fixture = Fixture::review()?;
            let session = CleanupSession::capture(&fixture.active_directory()?)?;
            if commented {
                fixture.comment(1, "one\ntwo\n", "First inline comment\n#| Literal text")?;
                fixture.comment(3, "three", "Second inline comment")?;
            }
            fixture.configure("editor_mode", json!("prepend"))?;
            fixture.configure(
                "editor_body",
                json!(format!(
                    "{SUMMARY}\n  #| This reference is not the summary\n"
                )),
            )?;
            fixture.configure("observe_sessions", json!([session.directory]))?;
            let checkout = ReviewFlowCheckout::capture(&fixture)?;
            let active = fs::read(fixture.state.join("active.json"))?;
            success(&fixture.execute(
                review_flow_command(&fixture, &session.directory, "head", &[action]),
                input,
            )?);
            review_flow_assert_submission(&fixture, &session, event, state, commented)?;
            assert_eq!(fs::read(fixture.state.join("active.json"))?, active);
            checkout.assert_kept(&fixture)?;
            assert_review_guard_refusal(
                &fixture,
                review_flow_command(&fixture, &session.directory, "head", &["submit-comment"]),
            )?;
            assert_eq!(fixture.posts()?.len(), 1);
        }
    }
    Ok(())
}

#[test]
fn review_flow_comment_submission_accepts_base_context_without_a_selection() -> Result<()> {
    for (action, input) in [("finish", "2\n"), ("submit-comment", "")] {
        let (fixture, directory) = navigation_session()?;
        let session = CleanupSession::capture(&directory)?;
        let before = fixture.draft()?;
        success(&fixture.execute(
            review_flow_command(&fixture, &directory, "base", &[action]),
            input,
        )?);
        let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &before, "COMMENTED")?;
        session.assert_cleaned(&receipt)?;
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.posts()?[0]["payload"]["event"], "COMMENT");
        assert_eq!(fixture.posts()?[0]["payload"]["comments"][0]["path"], FILE);
        assert_eq!(fixture.editors()?.len(), 1);
    }
    Ok(())
}

#[test]
fn review_flow_blank_summary_or_editor_failure_keeps_draft_and_snapshots() -> Result<()> {
    for commented in [false, true] {
        let fixture = Fixture::review()?;
        let session = CleanupSession::capture(&fixture.active_directory()?)?;
        if commented {
            fixture.comment(1, "one", "Do not abandon this inline comment")?;
        }
        for (mode, body) in [
            ("write", " \n\t"),
            ("unchanged", "Not written"),
            ("fail", SUMMARY),
        ] {
            fixture.configure("editor_mode", json!(mode))?;
            fixture.configure("editor_body", json!(body))?;
            for (action, input) in [
                ("finish", "2\n"),
                ("finish", "3\n"),
                ("finish", "4\n"),
                ("submit-comment", ""),
            ] {
                let before = NavigationCheckpoint::capture(&fixture)?;
                let archived = archives(session.directory.parent().unwrap())?;
                let editors = fixture.editors()?.len();
                let output = fixture.execute(
                    review_flow_command(&fixture, &session.directory, "head", &[action]),
                    input,
                )?;
                if mode == "fail" {
                    failure(&output);
                } else {
                    success(&output);
                }
                assert_eq!(fixture.editors()?.len(), editors + 1);
                let after = NavigationCheckpoint::capture(&fixture)?;
                assert_eq!(after.head, before.head);
                assert_eq!(after.active, before.active);
                assert_eq!(after.drafts, before.drafts);
                assert_eq!(archives(session.directory.parent().unwrap())?, archived);
                session.assert_kept()?;
                assert!(fixture.posts()?.is_empty());
            }
        }
    }
    Ok(())
}

#[test]
fn review_flow_comment_submission_still_requires_current_checkout_and_remote_revision() -> Result<()>
{
    for drift in ["checkout", "remote"] {
        let fixture = Fixture::review()?;
        fixture.comment(1, "one", "Keep the stale review")?;
        let directory = fixture.active_directory()?;
        if drift == "checkout" {
            fixture.nonempty_child("Leave the reviewed checkout")?;
        } else {
            fixture.configure("head", json!(fixture.base))?;
        }
        let before = cleanup_contents(&fixture.state)?;
        let checkout = ReviewFlowCheckout::capture(&fixture)?;
        for (action, input) in [
            ("finish", "2\n"),
            ("finish", "3\n"),
            ("finish", "4\n"),
            ("submit-comment", ""),
        ] {
            failure(&fixture.execute(
                review_flow_command(&fixture, &directory, "head", &[action]),
                input,
            )?);
            assert_eq!(cleanup_contents(&fixture.state)?, before);
            assert!(fixture.posts()?.is_empty());
            assert!(fixture.editors()?.is_empty());
        }
        checkout.assert_kept(&fixture)?;
    }
    Ok(())
}

#[test]
fn review_flow_comment_lost_response_and_delayed_acceptance_recover_without_reposting() -> Result<()>
{
    for (mode, action, input) in [
        ("lost_response", "finish", "2\n"),
        ("delayed_acceptance", "submit-comment", ""),
    ] {
        let fixture = Fixture::review()?;
        fixture.comment(1, "one\ntwo\n", "Recover this exact inline comment")?;
        let session = CleanupSession::capture(&fixture.active_directory()?)?;
        fixture.configure("post", json!(mode))?;
        failure(&fixture.execute(
            review_flow_command(&fixture, &session.directory, "head", &[action]),
            input,
        )?);
        let uncertain = fixture.draft()?;
        assert_eq!(uncertain["attempt"]["payload"]["event"], "COMMENT");
        assert!(uncertain["submitted"].is_null());
        session.assert_kept()?;
        failure(&fixture.run(&["submit-comment"], "")?);
        assert_eq!(fixture.draft()?, uncertain);
        if mode == "delayed_acceptance" {
            success(&fixture.saved(&["recover", "--retry"], "yes\n")?);
            assert_eq!(
                fixture.draft()?["retry_history"],
                json!([uncertain["attempt"]])
            );
            fixture.accept_delayed_review()?;
        }
        let before = fixture.draft()?;
        fixture.nonempty_child("Recovery away from the reviewed checkout")?;
        fixture.configure("deny_discovery", json!(true))?;
        success(&fixture.saved(&["recover"], "")?);
        let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &before, "COMMENTED")?;
        session.assert_cleaned(&receipt)?;
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.editors()?.len(), 1);
        success(&fixture.saved(&["recover"], "")?);
        assert_eq!(fixture.posts()?.len(), 1);
    }
    Ok(())
}

#[test]
fn review_flow_own_pr_allows_comment_but_not_approve_or_request_changes() -> Result<()> {
    for (action, input) in [("finish", "2\n"), ("submit-comment", "")] {
        let fixture = Fixture::review()?;
        fixture.configure("author", json!("ReViEwEr"))?;
        fixture.comment(1, "one", "Author's own comment")?;
        let directory = fixture.active_directory()?;
        let before = cleanup_contents(&fixture.state)?;
        for (refused, choice) in [
            ("finish", "3\n"),
            ("finish", "4\n"),
            ("approve", ""),
            ("request-changes", ""),
        ] {
            let output = fixture.execute(
                review_flow_command(&fixture, &directory, "head", &[refused]),
                choice,
            )?;
            failure(&output);
            assert!(describe(&output).contains("your own PR"));
            assert_eq!(cleanup_contents(&fixture.state)?, before);
            assert!(fixture.posts()?.is_empty());
            assert!(fixture.editors()?.is_empty());
        }
        success(&fixture.execute(
            review_flow_command(&fixture, &directory, "head", &[action]),
            input,
        )?);
        assert_eq!(fixture.posts()?.len(), 1);
        assert_eq!(fixture.posts()?[0]["payload"]["event"], "COMMENT");
        assert_eq!(fixture.draft()?["submitted"]["state"], "COMMENTED");
    }
    Ok(())
}

fn run_review_flow_terminal(scenario: &str) -> Result<()> {
    let (fixture, directory) = navigation_session()?;
    let session = CleanupSession::capture(&directory)?;
    let before = NavigationCheckpoint::capture(&fixture)?;
    let draft = fixture.draft()?;
    let checkout = ReviewFlowCheckout::capture(&fixture)?;
    navigation_clear_calls(&fixture)?;
    let transcript = fixture.root.join("review-terminal-output.txt");
    let output = File::create(&transcript)?;
    let mut command = fixture.command(env!("CARGO_BIN_EXE_zpr-test-helper"));
    command
        .arg("review-terminal")
        .arg(env!("CARGO_BIN_EXE_zed-pr-review"))
        .arg(scenario)
        .env("TERM", "xterm-256color")
        .env("ZED_FILE", directory.join("head").join(FILE))
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    let mut child = command
        .spawn()
        .context("Cannot start the Rust review PTY driver")?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // Reap the Rust driver's session, including any CLI or editor left on timeout.
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = fs::read_to_string(transcript)?;
    ensure!(
        status.is_some(),
        "Rust review PTY driver {scenario} exceeded 45 seconds:\n{output}"
    );
    ensure!(
        status.unwrap().success(),
        "Rust review PTY driver {scenario} failed:\n{output}"
    );
    for call in fixture.calls()? {
        ensure!(
            call.get("unexpected").is_none(),
            "Offline gh rejected: {call}"
        );
    }
    match scenario {
        "1" => {
            review_flow_assert_cancelled(&session)?;
            assert_eq!(archives(directory.parent().unwrap())?, vec![draft]);
            assert!(fixture.drafts()?.is_empty());
            assert!(!fixture.state.join("active.json").exists());
            assert!(fixture.calls()?.is_empty());
            assert!(fixture.editors()?.is_empty());
        }
        "2" | "3" | "4" | "down-comment" | "up-request" => {
            let (event, state) = match scenario {
                "2" | "down-comment" => ("COMMENT", "COMMENTED"),
                "3" => ("APPROVE", "APPROVED"),
                _ => ("REQUEST_CHANGES", "CHANGES_REQUESTED"),
            };
            let receipt = cleanup_assert_receipt(&fixture.draft_path()?, &draft, state)?;
            session.assert_cleaned(&receipt)?;
            assert_eq!(fixture.posts()?.len(), 1);
            assert_eq!(
                fixture.posts()?[0]["payload"],
                json!({
                    "event": event, "body": SUMMARY, "commit_id": fixture.head,
                    "comments": [{"path": FILE, "line": 1, "side": "RIGHT", "body": draft["comments"][0]["body"]}]
                })
            );
            assert_eq!(fixture.editors()?.len(), 1);
        }
        "editor-sigint" | "editor-sigterm" | "editor-sighup" => {
            before.assert_unchanged(&fixture)?;
            session.assert_kept()?;
            assert!(archives(directory.parent().unwrap())?.is_empty());
            let calls = fixture.calls()?;
            assert!(
                !calls.is_empty(),
                "The selected submission must reach discovery before the editor"
            );
            // A second local finish acquires both the workspace and draft locks.
            // It must succeed after the killed CLI and paused editor are gone.
            success(&fixture.execute(
                review_flow_command(&fixture, &directory, "head", &["finish"]),
                "q\n",
            )?);
            assert_eq!(fixture.calls()?, calls);
            before.assert_unchanged(&fixture)?;
            session.assert_kept()?;
        }
        _ => {
            before.assert_unchanged(&fixture)?;
            session.assert_kept()?;
            assert!(fixture.calls()?.is_empty());
        }
    }
    assert!(fixture.zed_calls()?.is_empty());
    checkout.assert_kept(&fixture)?;
    Ok(())
}

#[test]
fn review_flow_terminal_editor_sigint_terminates_cli_without_losing_draft() -> Result<()> {
    run_review_flow_terminal("editor-sigint")
}

#[test]
fn review_flow_terminal_editor_sigterm_terminates_cli_without_losing_draft() -> Result<()> {
    run_review_flow_terminal("editor-sigterm")
}

#[test]
fn review_flow_terminal_editor_sighup_terminates_cli_without_losing_draft() -> Result<()> {
    run_review_flow_terminal("editor-sighup")
}

#[test]
fn review_flow_terminal_dismissals_restore_terminal_and_keep_draft() -> Result<()> {
    for scenario in ["quit", "escape", "ctrl-c", "enter", "invalid"] {
        run_review_flow_terminal(scenario)?;
    }
    Ok(())
}

#[test]
fn review_flow_terminal_numbered_choices_restore_terminal_before_action() -> Result<()> {
    for scenario in ["1", "2", "3", "4"] {
        run_review_flow_terminal(scenario)?;
    }
    Ok(())
}

#[test]
fn review_flow_terminal_arrows_and_enter_submit_the_selected_action() -> Result<()> {
    for scenario in ["down-comment", "up-request"] {
        run_review_flow_terminal(scenario)?;
    }
    Ok(())
}

#[test]
fn ready_rejects_base_ref_or_node_id_drift_after_the_ready_baseline() -> Result<()> {
    for (field, changed) in [("base_ref", "release"), ("node_id", "PR_replaced17")] {
        let fixture = Fixture::new()?;
        // Discovery is GET #1, the ready baseline is #2, and the write guard is #3.
        fixture.configure("pr_read_changes", json!({"3": {(field): changed}}))?;
        failure(&fixture.run_ready(&["--pr", PR_URL, "--reviewer", "alice"])?);
        let control = fixture.control()?;
        assert!(control["pr_reads"].as_u64().unwrap() >= 3);
        assert_eq!(control[field], changed);
        assert_eq!(
            control["base"], fixture.base,
            "A branch can change without its SHA changing"
        );
        assert_eq!(control["head"], fixture.head);
        fixture.assert_ready_mutations(&[])?;
        fixture.assert_stateless_ready()?;
    }
    Ok(())
}
