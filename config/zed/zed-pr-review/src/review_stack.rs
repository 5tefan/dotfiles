//! Manage a local `jj` review stack with one squashed revision per PR.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::github::Client;
use crate::model::{ChangedFile, PullRequest};
use crate::stack::Stack;
use crate::store::{Draft, draft_key};

pub const BOOKMARK_PREFIX: &str = "review/pr-";
const META_VERSION: u32 = 1;
const REVIEW_AUTHOR_EMAIL: &str = "review@zed-pr-review.local";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReviewStackMeta {
    pub version: u32,
    pub previous_commit: String,
    pub pr_numbers: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReviewEntry {
    pub version: u32,
    pub bookmark: String,
    pub change_id: String,
    pub commit_id: String,
    pub parent_commit_id: String,
    pub pr: PullRequest,
    pub draft_key: String,
    pub files: Vec<ChangedFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentRevision {
    pub change_id: String,
    pub commit_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewStatus {
    pub pr_number: u64,
    pub expected_commit_id: String,
    pub pending_comments: usize,
    pub submitted_state: Option<String>,
}

pub fn bookmark_name(number: u64) -> String {
    format!("{BOOKMARK_PREFIX}{number}")
}

fn reviews_dir(state: &Path) -> PathBuf {
    state.join("reviews")
}

fn entry_path(state: &Path, number: u64) -> PathBuf {
    reviews_dir(state).join(format!("pr-{number}.json"))
}

fn stack_meta_path(state: &Path) -> PathBuf {
    reviews_dir(state).join("stack.json")
}

/// Build or refresh the local `jj` review stack for the connected PR stack
/// containing `target`, then switch `@` to `target`'s review revision.
pub fn open_stack(client: &Client, target: &PullRequest, state: &Path) -> Result<ReviewEntry> {
    let stack = Stack::discover(client, target)?;
    let dir = reviews_dir(state);
    fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create review stack directory {}", dir.display()))?;

    let existing_meta = load_meta(state).ok().flatten();
    let current_is_review = current_review(&client.root, state).ok().flatten().is_some();
    let previous_commit = match (&existing_meta, current_is_review) {
        (Some(meta), true) if !meta.previous_commit.is_empty() => meta.previous_commit.clone(),
        _ => client.checkout_commit()?,
    };

    let mut built: BTreeMap<u64, ReviewEntry> = BTreeMap::new();
    let mut obsolete_changes: Vec<String> = Vec::new();
    let order = stack.topological_order();

    for stack_entry in &order {
        let pr = &stack_entry.pr;
        println!("Preparing PR #{} — {}", pr.number, single_line(&pr.title));
        client.fetch_pr(pr)?;
        let merge_base = client.merge_base(pr)?;

        let (parent_commit_id, behind_parent) = if let Some(parent_stack_entry) =
            stack.parent_of(pr.number)
            && let Some(parent_review) = built.get(&parent_stack_entry.pr.number)
        {
            if merge_base.eq_ignore_ascii_case(&parent_stack_entry.pr.head_sha) {
                (parent_review.commit_id.clone(), None)
            } else {
                (merge_base.clone(), Some(parent_stack_entry.pr.number))
            }
        } else {
            (merge_base.clone(), None)
        };

        let existing = load_entry(state, pr.number).ok().flatten();
        if let Some(existing) = existing
            && can_reuse(&client.root, &existing, pr, &parent_commit_id)?
        {
            built.insert(pr.number, existing);
            continue;
        } else if let Some(existing) = load_entry(state, pr.number).ok().flatten() {
            obsolete_changes.push(existing.change_id);
        }

        let files = client.files(pr.number)?;
        let bookmark = bookmark_name(pr.number);
        let (change_id, commit_id) = create_review_revision(
            &client.root,
            &parent_commit_id,
            &pr.head_sha,
            &bookmark,
            pr,
            behind_parent,
        )?;
        let entry = ReviewEntry {
            version: META_VERSION,
            bookmark,
            change_id,
            commit_id,
            parent_commit_id,
            pr: pr.clone(),
            draft_key: draft_key(pr),
            files,
        };
        write_json_atomic(&entry_path(state, pr.number), &entry)?;
        built.insert(pr.number, entry);
    }

    let mut pr_numbers: BTreeSet<u64> = existing_meta
        .map(|meta| meta.pr_numbers.into_iter().collect())
        .unwrap_or_default();
    pr_numbers.extend(built.keys().copied());
    let meta = ReviewStackMeta {
        version: META_VERSION,
        previous_commit: previous_commit.clone(),
        pr_numbers: pr_numbers.into_iter().collect(),
    };
    write_json_atomic(&stack_meta_path(state), &meta)?;

    let target_entry = built
        .get(&target.number)
        .context("Target PR missing from built review stack")?
        .clone();
    edit_change(&client.root, &target_entry.change_id)?;

    // Abandon any superseded review revisions now that @ has moved to the target.
    for old_change in obsolete_changes {
        if old_change != target_entry.change_id {
            let _ = abandon_change(&client.root, &old_change);
        }
    }

    println!(
        "Checked out {} (PR #{}).",
        target_entry.bookmark, target.number
    );
    println!("Previous work is preserved. Return with: jj edit {previous_commit}");
    println!("If that revision is immutable, use: jj new {previous_commit}");
    Ok(target_entry)
}

/// Inspect `jj`'s `@` and return the matching `ReviewEntry` if `@` is on a review revision.
pub fn current_review(root: &Path, state: &Path) -> Result<Option<(ReviewEntry, CurrentRevision)>> {
    let entries = load_all_entries(state);
    if entries.is_empty() {
        return Ok(None);
    }
    let current = current_revision(root)?;
    let matched = entries
        .into_iter()
        .find(|entry| entry.change_id == current.change_id);
    Ok(matched.map(|entry| (entry, current)))
}

/// Tear down all local `review/pr-*` revisions and bookmarks, remove review metadata,
/// and return `@` to the saved `previous_commit`.
pub fn close_stack(root: &Path, state: &Path) -> Result<()> {
    let meta = load_meta(state).ok().flatten();
    let entries = load_all_entries(state);

    // Move @ off the review stack before abandoning review revisions.
    if let Some(meta) = &meta
        && !meta.previous_commit.is_empty()
        && commit_exists(root, &meta.previous_commit)?
    {
        if is_immutable(root, &meta.previous_commit)? {
            run_jj(
                root,
                &["new", &meta.previous_commit],
                "create a new working copy on the previous immutable commit",
            )?;
        } else {
            run_jj(
                root,
                &["edit", &meta.previous_commit],
                "return to the previous working copy",
            )?;
        }
        println!("Returned to previous work ({}).", meta.previous_commit);
    } else if current_review(root, state).ok().flatten().is_some() {
        let _ = run_jj(root, &["new", "trunk()"], "leave the review stack");
    }

    let mut bookmarks: BTreeSet<String> = entries.iter().map(|e| e.bookmark.clone()).collect();
    for existing in list_review_bookmarks(root) {
        bookmarks.insert(existing);
    }
    for bookmark in &bookmarks {
        let _ = run_jj(
            root,
            &["bookmark", "forget", bookmark],
            "forget review bookmark",
        );
    }

    for entry in &entries {
        let _ = abandon_change(root, &entry.change_id);
        let _ = fs::remove_file(entry_path(state, entry.pr.number));
    }
    let _ = fs::remove_file(stack_meta_path(state));

    println!(
        "Closed review stack ({} PR{} cleaned up). Drafts and receipts are preserved.",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Fast, read-only status lookup keyed by `jj` `change_id`, used by `jj l`.
pub fn status_map(state: &Path) -> HashMap<String, ReviewStatus> {
    let entries = load_all_entries(state);
    let mut map = HashMap::with_capacity(entries.len());
    for entry in entries {
        let draft_file = state.join(&entry.draft_key).join("draft.json");
        let (pending_comments, submitted_state) = match fs::read(&draft_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Draft>(&bytes).ok())
        {
            Some(draft) => (
                draft.comments.len(),
                draft.submitted.map(|receipt| receipt.state),
            ),
            None => (0, None),
        };
        map.insert(
            entry.change_id,
            ReviewStatus {
                pr_number: entry.pr.number,
                expected_commit_id: entry.commit_id,
                pending_comments,
                submitted_state,
            },
        );
    }
    map
}

pub fn current_revision(root: &Path) -> Result<CurrentRevision> {
    let output = run_jj(
        root,
        &[
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            r#"change_id ++ "\t" ++ commit_id ++ "\n""#,
        ],
        "inspect jj @ revision",
    )?;
    let line = output
        .lines()
        .next()
        .context("jj log -r @ produced no output")?;
    let (change_id, commit_id) = line
        .split_once('\t')
        .context("Unexpected jj log -r @ output format")?;
    Ok(CurrentRevision {
        change_id: change_id.trim().to_owned(),
        commit_id: commit_id.trim().to_owned(),
    })
}

fn create_review_revision(
    root: &Path,
    parent_commit_id: &str,
    head_sha: &str,
    bookmark: &str,
    pr: &PullRequest,
    behind_parent: Option<u64>,
) -> Result<(String, String)> {
    let before = child_change_ids(root, parent_commit_id)?;
    let author_name = format!("PR Review ({})", sanitize_author(&pr.author));
    let description = format_description(pr, behind_parent);
    let name_config = format!(
        "user.name={}",
        serde_json::to_string(&author_name).context("encode review author name")?
    );
    let email_config = format!(
        "user.email={}",
        serde_json::to_string(REVIEW_AUTHOR_EMAIL).context("encode review author email")?
    );

    run_jj(
        root,
        &[
            "--config",
            &name_config,
            "--config",
            &email_config,
            "new",
            parent_commit_id,
            "--no-edit",
            "-m",
            &description,
        ],
        "create squashed review revision",
    )?;

    let after = child_change_ids(root, parent_commit_id)?;
    let mut created: Vec<String> = after.difference(&before).cloned().collect();
    ensure!(
        created.len() == 1,
        "Expected one newly created review revision under {parent_commit_id}, found {}",
        created.len()
    );
    let change_id = created.pop().unwrap();
    let change_rev = format!("change_id(\"{change_id}\")");

    run_jj(
        root,
        &["restore", "--from", head_sha, "--to", &change_rev],
        "populate review revision tree from PR head",
    )?;
    run_jj(
        root,
        &[
            "bookmark",
            "set",
            bookmark,
            "-r",
            &change_rev,
            "--allow-backwards",
        ],
        "point review bookmark at revision",
    )?;
    let commit_id = run_jj(
        root,
        &["log", "-r", &change_rev, "--no-graph", "-T", "commit_id"],
        "read created review commit ID",
    )?
    .trim()
    .to_owned();
    ensure!(!commit_id.is_empty(), "Created review commit ID is empty");

    Ok((change_id, commit_id))
}

fn can_reuse(
    root: &Path,
    existing: &ReviewEntry,
    pr: &PullRequest,
    parent_commit_id: &str,
) -> Result<bool> {
    if existing.version != META_VERSION
        || !existing.pr.same_revision(pr)
        || existing.parent_commit_id != parent_commit_id
        || existing.bookmark != bookmark_name(pr.number)
    {
        return Ok(false);
    }
    let revset = format!("change_id(\"{}\") & ::visible_heads()", existing.change_id);
    let Ok(output) = run_jj(
        root,
        &[
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            r#"commit_id ++ "\t" ++ divergent ++ "\n""#,
        ],
        "check existing review revision",
    ) else {
        return Ok(false);
    };
    let lines: Vec<_> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() != 1 {
        return Ok(false);
    }
    let Some((commit_id, divergent)) = lines[0].split_once('\t') else {
        return Ok(false);
    };
    if commit_id.trim() != existing.commit_id || divergent.trim() != "false" {
        return Ok(false);
    }
    let _ = run_jj(
        root,
        &[
            "bookmark",
            "set",
            &existing.bookmark,
            "-r",
            &format!("change_id(\"{}\")", existing.change_id),
            "--allow-backwards",
        ],
        "ensure review bookmark points to existing revision",
    );
    Ok(true)
}

fn child_change_ids(root: &Path, parent_commit_id: &str) -> Result<HashSet<String>> {
    let revset = format!("children({parent_commit_id})");
    let output = run_jj(
        root,
        &[
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            r#"change_id ++ "\n""#,
        ],
        "list child revisions",
    )?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn edit_change(root: &Path, change_id: &str) -> Result<()> {
    let revset = format!("change_id(\"{change_id}\")");
    run_jj(root, &["edit", &revset], "check out review revision")?;
    Ok(())
}

fn abandon_change(root: &Path, change_id: &str) -> Result<()> {
    let revset = format!("change_id(\"{change_id}\") & ::visible_heads()");
    let output = run_jj(
        root,
        &["log", "-r", &revset, "--no-graph", "-T", "change_id"],
        "check review revision before abandon",
    )?;
    if output.trim().is_empty() {
        return Ok(());
    }
    run_jj(root, &["abandon", &revset], "abandon review revision")?;
    Ok(())
}

fn commit_exists(root: &Path, commit_id: &str) -> Result<bool> {
    let output = jj_command(root)
        .args(["log", "-r", commit_id, "--no-graph", "-T", "commit_id"])
        .output()
        .context("Cannot run jj to check previous commit")?;
    Ok(output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn is_immutable(root: &Path, commit_id: &str) -> Result<bool> {
    let output = run_jj(
        root,
        &["log", "-r", commit_id, "--no-graph", "-T", "immutable"],
        "check if previous commit is immutable",
    )?;
    Ok(output.trim() == "true")
}

fn list_review_bookmarks(root: &Path) -> Vec<String> {
    let Ok(output) = run_jj(
        root,
        &[
            "bookmark",
            "list",
            "glob:review/pr-*",
            "-T",
            r#"name ++ "\n""#,
        ],
        "list review bookmarks",
    ) else {
        return Vec::new();
    };
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(BOOKMARK_PREFIX))
        .map(ToOwned::to_owned)
        .collect()
}

fn format_description(pr: &PullRequest, behind_parent: Option<u64>) -> String {
    let title = single_line(&pr.title);
    let suffix = match behind_parent {
        Some(parent) => format!(" [behind #{parent}]"),
        None => String::new(),
    };
    format!(
        "PR #{}: {title}{suffix}\n\n{}\nhead: {}\nbase: {}",
        pr.number, pr.url, pr.head_sha, pr.base_sha
    )
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_author(author: &str) -> String {
    let cleaned: String = author
        .chars()
        .filter(|c| !c.is_control() && *c != '<' && *c != '>')
        .collect();
    if cleaned.trim().is_empty() {
        "unknown".into()
    } else {
        cleaned.trim().to_owned()
    }
}

fn load_meta(state: &Path) -> Result<Option<ReviewStackMeta>> {
    let path = stack_meta_path(state);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("Invalid review stack metadata at {}", path.display())
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn load_entry(state: &Path, number: u64) -> Result<Option<ReviewEntry>> {
    let path = entry_path(state, number);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("Invalid review entry at {}", path.display())
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn load_all_entries(state: &Path) -> Vec<ReviewEntry> {
    let dir = reviews_dir(state);
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in read_dir.flatten() {
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("pr-") || !name.ends_with(".json") {
            continue;
        }
        if let Ok(bytes) = fs::read(item.path())
            && let Ok(entry) = serde_json::from_slice::<ReviewEntry>(&bytes)
        {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.pr.number);
    entries
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("No parent directory for {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Cannot create directory {}", parent.display()))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Cannot create temp file in {}", parent.display()))?;
    serde_json::to_writer_pretty(temp.as_file_mut(), value)?;
    temp.as_file_mut().write_all(b"\n")?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Cannot persist {}", path.display()))?;
    Ok(())
}

fn jj_command(root: &Path) -> Command {
    let mut command = Command::new("jj");
    command
        .current_dir(root)
        .env("JJ_PAGER", "cat")
        .env("PAGER", "cat")
        .args(["--no-pager", "--color=never"]);
    command
}

fn run_jj(root: &Path, args: &[&str], action: &str) -> Result<String> {
    let output = jj_command(root)
        .args(args)
        .output()
        .with_context(|| format!("Cannot run jj to {action}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to {action} ({}): {}", output.status, stderr.trim());
    }
    String::from_utf8(output.stdout).context("jj output was not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Comment;
    use crate::store::{Receipt, Store};
    use tempfile::TempDir;

    fn pr(number: u64, head_sha: &str, base_sha: &str) -> PullRequest {
        PullRequest {
            host: "github.com".into(),
            repo: "owner/repo".into(),
            number,
            url: format!("https://github.com/owner/repo/pull/{number}"),
            title: format!("PR {number} title"),
            head_sha: head_sha.into(),
            base_sha: base_sha.into(),
            head_repo: "owner/repo".into(),
            head_ref: format!("feat/pr-{number}"),
            author: "alice".into(),
            reviewer: "stefan".into(),
        }
    }

    fn init_repo() -> Result<(TempDir, PathBuf)> {
        let temp = TempDir::new()?;
        let root = temp.path().join("repo");
        fs::create_dir_all(&root)?;
        run_jj(&root, &["git", "init", "--colocate"], "init jj repo")?;
        run_jj(
            &root,
            &["config", "set", "--repo", "user.name", "Stefan"],
            "set user.name",
        )?;
        run_jj(
            &root,
            &[
                "config",
                "set",
                "--repo",
                "user.email",
                "stefan@example.com",
            ],
            "set user.email",
        )?;
        fs::write(root.join("base.txt"), "base\n")?;
        run_jj(&root, &["commit", "-m", "base"], "commit base")?;
        run_jj(
            &root,
            &["bookmark", "create", "main", "-r", "@-"],
            "create main",
        )?;
        Ok((temp, root))
    }

    #[test]
    fn create_reuse_status_and_close_review_revisions_in_real_jj() -> Result<()> {
        let (_temp, root) = init_repo()?;
        let state = root.join("scratch/zed-pr-review");
        fs::create_dir_all(&state)?;

        // Create two commits for PR #101 (to verify squashing into one review revision)
        // and one commit on top for PR #102.
        let main_sha = run_jj(
            &root,
            &["log", "-r", "main", "--no-graph", "-T", "commit_id"],
            "main",
        )?
        .trim()
        .to_owned();
        fs::write(root.join("a.txt"), "one\n")?;
        run_jj(&root, &["commit", "-m", "pr101 part 1"], "commit 1")?;
        fs::write(root.join("b.txt"), "two\n")?;
        run_jj(&root, &["commit", "-m", "pr101 part 2"], "commit 2")?;
        let pr101_head = run_jj(
            &root,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            "pr101",
        )?
        .trim()
        .to_owned();

        fs::write(root.join("c.txt"), "three\n")?;
        run_jj(&root, &["commit", "-m", "pr102"], "commit 3")?;
        let pr102_head = run_jj(
            &root,
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            "pr102",
        )?
        .trim()
        .to_owned();

        // Go back to a fresh working copy on main as the user's "previous work".
        run_jj(&root, &["new", "main", "-m", "my wip"], "wip")?;
        let previous_commit = run_jj(
            &root,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            "wip id",
        )?
        .trim()
        .to_owned();

        let pr101 = pr(101, &pr101_head, &main_sha);
        let pr102 = pr(102, &pr102_head, &pr101_head);

        // Build review revision 101 on top of main_sha.
        let (chg101, com101) = create_review_revision(
            &root,
            &main_sha,
            &pr101_head,
            &bookmark_name(101),
            &pr101,
            None,
        )?;
        let entry101 = ReviewEntry {
            version: META_VERSION,
            bookmark: bookmark_name(101),
            change_id: chg101.clone(),
            commit_id: com101.clone(),
            parent_commit_id: main_sha.clone(),
            pr: pr101.clone(),
            draft_key: draft_key(&pr101),
            files: vec![],
        };
        write_json_atomic(&entry_path(&state, 101), &entry101)?;

        // Build review revision 102 on top of review revision 101.
        let (chg102, com102) = create_review_revision(
            &root,
            &com101,
            &pr102_head,
            &bookmark_name(102),
            &pr102,
            None,
        )?;
        let entry102 = ReviewEntry {
            version: META_VERSION,
            bookmark: bookmark_name(102),
            change_id: chg102.clone(),
            commit_id: com102.clone(),
            parent_commit_id: com101.clone(),
            pr: pr102.clone(),
            draft_key: draft_key(&pr102),
            files: vec![],
        };
        write_json_atomic(&entry_path(&state, 102), &entry102)?;
        write_json_atomic(
            &stack_meta_path(&state),
            &ReviewStackMeta {
                version: META_VERSION,
                previous_commit: previous_commit.clone(),
                pr_numbers: vec![101, 102],
            },
        )?;

        // Reuse check succeeds when unchanged.
        assert!(can_reuse(&root, &entry101, &pr101, &main_sha)?);
        assert!(can_reuse(&root, &entry102, &pr102, &com101)?);

        // Author is distinct from user's own email so mine() excludes review commits.
        let mine_revs = run_jj(
            &root,
            &[
                "log",
                "-r",
                "mine()",
                "--no-graph",
                "-T",
                r#"change_id ++ "\n""#,
            ],
            "mine",
        )?;
        assert!(!mine_revs.contains(&chg101));
        assert!(!mine_revs.contains(&chg102));

        // Edit PR 101, verify working tree has both a.txt and b.txt (squashed) and not c.txt.
        edit_change(&root, &chg101)?;
        assert_eq!(fs::read_to_string(root.join("a.txt"))?, "one\n");
        assert_eq!(fs::read_to_string(root.join("b.txt"))?, "two\n");
        assert!(!root.join("c.txt").exists());
        let (matched, cur) = current_review(&root, &state)?.expect("should match PR 101");
        assert_eq!(matched.pr.number, 101);
        assert_eq!(cur.commit_id, com101);

        // Step forward with normal `jj edit @+` -> lands on PR 102 with c.txt!
        run_jj(&root, &["edit", "@+"], "step forward")?;
        assert_eq!(fs::read_to_string(root.join("c.txt"))?, "three\n");
        let (matched, _) = current_review(&root, &state)?.expect("should match PR 102");
        assert_eq!(matched.pr.number, 102);

        // Save a pending comment on PR 101 and an approval receipt on PR 102; verify status_map.
        let store101 = Store::open(&state, &pr101)?;
        let mut draft101 = Draft::new(pr101.clone());
        draft101.comments.push(Comment {
            path: "a.txt".into(),
            start_line: 1,
            line: 1,
            column: 1,
            selected_text: "one".into(),
            body: "looks good".into(),
        });
        store101.save(&draft101)?;

        let store102 = Store::open(&state, &pr102)?;
        let mut draft102 = Draft::new(pr102.clone());
        draft102.submitted = Some(Receipt {
            id: 999,
            url: "https://github.com/owner/repo/pull/102#pullrequestreview-999".into(),
            state: "APPROVED".into(),
        });
        store102.save(&draft102)?;

        let statuses = status_map(&state);
        assert_eq!(statuses[&chg101].pending_comments, 1);
        assert_eq!(statuses[&chg101].submitted_state, None);
        assert_eq!(
            statuses[&chg102].submitted_state.as_deref(),
            Some("APPROVED")
        );

        // Close stack returns @ to previous_commit and removes bookmarks + metadata.
        close_stack(&root, &state)?;
        let now = current_revision(&root)?;
        assert_eq!(now.commit_id, previous_commit);
        assert!(list_review_bookmarks(&root).is_empty());
        assert!(load_all_entries(&state).is_empty());
        assert!(load_meta(&state)?.is_none());

        Ok(())
    }
}
