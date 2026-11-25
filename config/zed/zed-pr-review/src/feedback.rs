//! Local-first GitHub review-thread feedback workflow for stacked jj PRs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::github::{self, BranchPrState, Client};
use crate::model::{ReviewThread, ThreadComment};
use crate::push::{self, StackBookmark};
use crate::store::{Store, private_directory};

const DRAFT_VERSION: u32 = 1;
const INDEX_VERSION: u32 = 1;
const BADGE_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplyReceipt {
    pub comment_id: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FeedbackAttempt {
    pub reply_body: Option<String>,
    pub resolve: bool,
    pub previous_comment_ids: Vec<String>,
    pub reply_posted: Option<ReplyReceipt>,
    pub resolved_confirmed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FeedbackReceipt {
    pub reply: Option<ReplyReceipt>,
    pub resolved: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FeedbackDraft {
    pub version: u32,
    pub host: String,
    pub repo: String,
    pub pr_number: u64,
    pub thread_id: String,
    pub bookmark: String,
    pub path: String,
    pub line: Option<u32>,
    pub baseline_comment_ids: Vec<String>,
    pub reply_body: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reply_agent: bool,
    pub resolve: bool,
    pub attempt: Option<FeedbackAttempt>,
    #[serde(default)]
    pub retry_history: Vec<FeedbackAttempt>,
    pub submitted: Option<FeedbackReceipt>,
}

impl FeedbackDraft {
    pub fn from_thread(host: &str, repo: &str, thread: &ReviewThread) -> Self {
        Self {
            version: DRAFT_VERSION,
            host: host.to_owned(),
            repo: repo.to_owned(),
            pr_number: thread.pr_number,
            thread_id: thread.id.clone(),
            bookmark: thread.bookmark.clone(),
            path: thread.path.clone(),
            line: thread.effective_line(),
            baseline_comment_ids: thread.comment_ids(),
            reply_body: None,
            reply_agent: false,
            resolve: false,
            attempt: None,
            retry_history: Vec::new(),
            submitted: None,
        }
    }

    pub fn has_staged_work(&self) -> bool {
        self.reply_body
            .as_deref()
            .is_some_and(|b| !b.trim().is_empty())
            || self.resolve
    }

    pub fn editable(&self) -> Result<()> {
        ensure!(
            self.attempt.is_none(),
            "A feedback publish attempt for PR #{} ({}) may already have reached GitHub. Run `zed-pr-review feedback recover` before editing or publishing again",
            self.pr_number,
            self.path
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IndexedGroup {
    pub bookmark: String,
    pub change_id: String,
    pub commit_id: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub pr_url: String,
    pub head_sha: String,
    pub threads: Vec<ReviewThread>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FeedbackIndex {
    pub version: u32,
    pub host: String,
    pub repo: String,
    pub groups: Vec<IndexedGroup>,
}

#[derive(Clone, Debug)]
pub struct IndexedThread {
    pub index: usize,
    pub group_index: usize,
    pub bookmark: String,
    pub change_id: String,
    pub commit_id: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub pr_url: String,
    pub head_sha: String,
    pub thread: ReviewThread,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnpushedBookmark {
    pub bookmark: String,
    pub pr_number: u64,
    pub local_commit_id: String,
    pub remote_head_sha: String,
}

#[derive(Clone, Debug, Default)]
pub struct PublishReport {
    pub published_replies: usize,
    pub resolved_threads: usize,
    pub skipped_stale: Vec<(u64, String, String)>,
    pub skipped_already_resolved: usize,
}

#[derive(Clone, Debug, Default)]
pub struct RecoverReport {
    pub recovered: usize,
    pub pending_retry: usize,
    pub retry_enabled: usize,
}

pub fn thread_draft_key(host: &str, repo: &str, pr_number: u64, thread_id: &str) -> String {
    let identity = format!(
        "{}\n{}\n{}\n{}",
        host.to_ascii_lowercase(),
        repo.to_ascii_lowercase(),
        pr_number,
        thread_id
    );
    format!("{:x}", Sha256::digest(identity))
}

pub fn feedback_root(state_dir: &Path) -> PathBuf {
    state_dir.join("feedback")
}

pub fn open_thread_store(
    feedback_dir: &Path,
    host: &str,
    repo: &str,
    pr_number: u64,
    thread_id: &str,
) -> Result<Store> {
    private_directory(feedback_dir)?;
    Store::at(&feedback_dir.join(thread_draft_key(host, repo, pr_number, thread_id)))
}

pub fn load_draft(store: &Store) -> Result<Option<FeedbackDraft>> {
    let path = store.directory.join("draft.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot read feedback draft"),
    };
    let draft: FeedbackDraft = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Invalid feedback draft at {}; no data was overwritten",
            path.display()
        )
    })?;
    ensure!(
        draft.version == DRAFT_VERSION,
        "Unsupported feedback draft version {}",
        draft.version
    );
    Ok(Some(draft))
}

pub fn save_draft(store: &Store, draft: &FeedbackDraft) -> Result<()> {
    store.write_json("draft.json", draft)
}

pub fn clear_draft(store: &Store) -> Result<()> {
    store.clear()
}

pub fn archive_draft(store: &Store, draft: &FeedbackDraft) -> Result<PathBuf> {
    let mut file = tempfile::Builder::new()
        .prefix("archive-")
        .suffix(".json")
        .tempfile_in(&store.directory)?;
    serde_json::to_writer_pretty(&mut file, draft)?;
    file.as_file().sync_all()?;
    let (_, path) = file.keep()?;
    Ok(path)
}

pub fn save_index(feedback_dir: &Path, index: &FeedbackIndex) -> Result<()> {
    let store = Store::at(feedback_dir)?;
    store.write_json("index.json", index)
}

pub fn load_index(feedback_dir: &Path) -> Result<Option<FeedbackIndex>> {
    let path = feedback_dir.join("index.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot read feedback index"),
    };
    let index: FeedbackIndex = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Invalid feedback index at {}; run `zed-pr-review feedback fetch` to refresh",
            path.display()
        )
    })?;
    ensure!(
        index.version == INDEX_VERSION,
        "Unsupported feedback index version {}",
        index.version
    );
    Ok(Some(index))
}

/// Read all saved feedback drafts under `feedback_dir` (read-only snapshot).
pub fn list_all_drafts(feedback_dir: &Path) -> Result<Vec<(PathBuf, FeedbackDraft)>> {
    if !feedback_dir.exists() {
        return Ok(Vec::new());
    }
    let mut drafts = Vec::new();
    for entry in fs::read_dir(feedback_dir).context("Cannot read feedback state directory")? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let draft_path = dir.join("draft.json");
        if !draft_path.is_file() {
            continue;
        }
        let bytes = fs::read(&draft_path)
            .with_context(|| format!("Cannot read {}", draft_path.display()))?;
        let draft: FeedbackDraft = serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid feedback draft at {}", draft_path.display()))?;
        if draft.version == DRAFT_VERSION {
            drafts.push((dir, draft));
        }
    }
    drafts.sort_by(|a, b| {
        a.1.pr_number
            .cmp(&b.1.pr_number)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.line.cmp(&b.1.line))
            .then_with(|| a.1.thread_id.cmp(&b.1.thread_id))
    });
    Ok(drafts)
}

/// Load a read-only map of `(pr_number, thread_id) -> FeedbackDraft` for display.
pub fn load_draft_map(feedback_dir: &Path) -> HashMap<(u64, String), FeedbackDraft> {
    let mut map = HashMap::new();
    if let Ok(drafts) = list_all_drafts(feedback_dir) {
        for (_, draft) in drafts {
            map.insert((draft.pr_number, draft.thread_id.clone()), draft);
        }
    }
    map
}

/// Fetch review threads for the current jj stack (or an explicit PR / revset).
pub fn fetch_stack_feedback(
    client: &Client,
    explicit_pr: Option<&str>,
    revset: Option<&str>,
) -> Result<FeedbackIndex> {
    if let Some(pr_spec) = explicit_pr {
        let (_, pr) = Client::resolve(
            &client.root,
            pr_spec,
            Some(&format!("{}/{}", client.host, client.repo)),
        )?;
        let threads = client.fetch_review_threads(pr.number, &pr.head_ref, &pr.head_sha)?;
        let (change_id, commit_id) =
            lookup_local_bookmark_ids(&client.root, &client.git_dir, &pr.head_ref)
                .unwrap_or_default();
        return Ok(FeedbackIndex {
            version: INDEX_VERSION,
            host: client.host.clone(),
            repo: client.repo.clone(),
            groups: vec![IndexedGroup {
                bookmark: pr.head_ref,
                change_id,
                commit_id,
                pr_number: pr.number,
                pr_title: pr.title,
                pr_url: pr.url,
                head_sha: pr.head_sha,
                threads,
            }],
        });
    }

    let bookmarks = push::discover_stack_bookmarks(&client.root, &client.git_dir, revset, true)?;
    let mut groups = Vec::new();
    for bm in bookmarks {
        let prs = client.list_prs_for_head(&bm.bookmark)?;
        let Some(pr) = prs.into_iter().find(|p| p.state == BranchPrState::Open) else {
            continue;
        };
        let threads = client.fetch_review_threads(pr.number, &bm.bookmark, &pr.head_sha)?;
        groups.push(IndexedGroup {
            bookmark: bm.bookmark,
            change_id: bm.change_id,
            commit_id: bm.commit_id,
            pr_number: pr.number,
            pr_title: pr.title,
            pr_url: pr.url,
            head_sha: pr.head_sha,
            threads,
        });
    }

    Ok(FeedbackIndex {
        version: INDEX_VERSION,
        host: client.host.clone(),
        repo: client.repo.clone(),
        groups,
    })
}

fn lookup_local_bookmark_ids(
    root: &Path,
    git_dir: &Path,
    bookmark: &str,
) -> Option<(String, String)> {
    let bookmarks = push::discover_stack_bookmarks(root, git_dir, None, true).ok()?;
    bookmarks
        .into_iter()
        .find(|b| b.bookmark == bookmark)
        .map(|b| (b.change_id, b.commit_id))
}

/// Flatten an index into 1-based visible threads.
/// By default (`include_all == false`), unresolved threads are shown, including
/// unresolved-but-outdated threads so comments are not missed after a rebase.
pub fn visible_threads(index: &FeedbackIndex, include_all: bool) -> Vec<IndexedThread> {
    let mut out = Vec::new();
    let mut next_index = 1usize;
    for (group_index, group) in index.groups.iter().enumerate() {
        for thread in &group.threads {
            if !include_all && thread.is_resolved {
                continue;
            }
            out.push(IndexedThread {
                index: next_index,
                group_index,
                bookmark: group.bookmark.clone(),
                change_id: group.change_id.clone(),
                commit_id: group.commit_id.clone(),
                pr_number: group.pr_number,
                pr_title: group.pr_title.clone(),
                pr_url: group.pr_url.clone(),
                head_sha: group.head_sha.clone(),
                thread: thread.clone(),
            });
            next_index += 1;
        }
    }
    out
}

pub fn thread_by_index(
    index: &FeedbackIndex,
    one_based: usize,
    include_all: bool,
) -> Result<IndexedThread> {
    let threads = visible_threads(index, include_all);
    ensure!(
        one_based > 0 && one_based <= threads.len(),
        "Invalid thread index {one_based}; expected 1..={}",
        threads.len()
    );
    Ok(threads[one_based - 1].clone())
}

pub fn thread_by_pr_and_index(
    index: &FeedbackIndex,
    pr_number: u64,
    one_based: usize,
    include_all: bool,
) -> Result<IndexedThread> {
    let threads: Vec<IndexedThread> = visible_threads(index, include_all)
        .into_iter()
        .filter(|t| t.pr_number == pr_number)
        .collect();
    ensure!(
        !threads.is_empty(),
        "No matching threads found for PR #{pr_number}"
    );
    ensure!(
        one_based > 0 && one_based <= threads.len(),
        "Invalid thread index {one_based} for PR #{pr_number}; expected 1..={}",
        threads.len()
    );
    Ok(threads[one_based - 1].clone())
}

pub fn thread_by_id(
    index: &FeedbackIndex,
    pr_number: Option<u64>,
    thread_id: &str,
    include_all: bool,
) -> Result<IndexedThread> {
    let matches: Vec<IndexedThread> = visible_threads(index, include_all)
        .into_iter()
        .filter(|t| t.thread.id == thread_id && pr_number.is_none_or(|n| t.pr_number == n))
        .collect();
    if matches.is_empty() {
        if !include_all
            && visible_threads(index, true)
                .into_iter()
                .any(|t| t.thread.id == thread_id && pr_number.is_none_or(|n| t.pr_number == n))
        {
            bail!("Thread {thread_id} is already resolved; pass --all to target resolved threads");
        }
        match pr_number {
            Some(n) => bail!("Thread {thread_id} not found on PR #{n}"),
            None => bail!("Thread {thread_id} not found in the feedback index"),
        }
    }
    ensure!(
        matches.len() == 1,
        "Thread {thread_id} matched multiple PRs; pass --pr <NUMBER>"
    );
    Ok(matches.into_iter().next().unwrap())
}

/// Stage or update a local reply for a review thread.
pub fn stage_reply(
    feedback_dir: &Path,
    host: &str,
    repo: &str,
    thread: &ReviewThread,
    body: &str,
    agent: bool,
) -> Result<FeedbackDraft> {
    let trimmed = body.trim();
    ensure!(!trimmed.is_empty(), "Reply body must not be empty");
    let store = open_thread_store(feedback_dir, host, repo, thread.pr_number, &thread.id)?;
    let mut draft =
        load_draft(&store)?.unwrap_or_else(|| FeedbackDraft::from_thread(host, repo, thread));
    draft.editable()?;
    draft.bookmark = thread.bookmark.clone();
    draft.path = thread.path.clone();
    draft.line = thread.effective_line();
    // Refresh baseline only if the user hasn't already staged work on an earlier snapshot.
    if !draft.has_staged_work() {
        draft.baseline_comment_ids = thread.comment_ids();
    }
    draft.reply_body = Some(trimmed.to_owned());
    draft.reply_agent = agent;
    draft.submitted = None;
    save_draft(&store, &draft)?;
    Ok(draft)
}

/// Stage or clear the local resolution flag for a review thread.
pub fn set_resolve(
    feedback_dir: &Path,
    host: &str,
    repo: &str,
    thread: &ReviewThread,
    resolve: bool,
) -> Result<Option<FeedbackDraft>> {
    let store = open_thread_store(feedback_dir, host, repo, thread.pr_number, &thread.id)?;
    let mut draft = match load_draft(&store)? {
        Some(existing) => existing,
        None if !resolve => return Ok(None),
        None => FeedbackDraft::from_thread(host, repo, thread),
    };
    draft.editable()?;
    draft.bookmark = thread.bookmark.clone();
    draft.path = thread.path.clone();
    draft.line = thread.effective_line();
    if !draft.has_staged_work() {
        draft.baseline_comment_ids = thread.comment_ids();
    }
    draft.resolve = resolve;
    if !draft.has_staged_work() && draft.retry_history.is_empty() && draft.submitted.is_none() {
        clear_draft(&store)?;
        return Ok(None);
    }
    if resolve {
        draft.submitted = None;
    }
    save_draft(&store, &draft)?;
    Ok(Some(draft))
}

/// Clear staged reply and resolution intent for a review thread.
pub fn clear_staged(
    feedback_dir: &Path,
    host: &str,
    repo: &str,
    thread: &ReviewThread,
) -> Result<bool> {
    let store = open_thread_store(feedback_dir, host, repo, thread.pr_number, &thread.id)?;
    let Some(mut draft) = load_draft(&store)? else {
        return Ok(false);
    };
    draft.editable()?;
    draft.reply_body = None;
    draft.reply_agent = false;
    draft.resolve = false;
    if draft.retry_history.is_empty() && draft.submitted.is_none() {
        clear_draft(&store)?;
    } else {
        save_draft(&store, &draft)?;
    }
    Ok(true)
}

/// Archive and clear all staged feedback drafts (used by `ctrl-shift-x` Discard).
pub fn discard_all_staged(feedback_dir: &Path) -> Result<usize> {
    let drafts = list_all_drafts(feedback_dir)?;
    for (_, draft) in &drafts {
        draft.editable()?;
    }
    let mut cleared = 0;
    for (dir, draft) in drafts {
        if !draft.has_staged_work() {
            continue;
        }
        let store = Store::at(&dir)?;
        let _ = archive_draft(&store, &draft)?;
        clear_draft(&store)?;
        cleared += 1;
    }
    Ok(cleared)
}

/// Build an editor template with `#|` reference lines showing the review thread context.
pub fn reply_template(thread: &ReviewThread, existing_reply: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(reply) = existing_reply {
        out.push_str(reply.trim_end());
        out.push_str("\n\n");
    } else {
        out.push_str("\n\n");
    }
    push_ref(
        &mut out,
        "Write your reply above. Leaving it blank cancels.",
    );
    push_ref(
        &mut out,
        "Lines starting with #| are removed before saving.",
    );
    push_ref(&mut out, "");
    let loc = match thread.effective_line() {
        Some(line) => format!("{}:{}", thread.path, line),
        None => thread.path.clone(),
    };
    push_ref(
        &mut out,
        &format!(
            "PR #{} ({}) — {loc}{}",
            thread.pr_number,
            thread.bookmark,
            if thread.is_outdated {
                " [outdated]"
            } else {
                ""
            }
        ),
    );
    for comment in &thread.comments {
        push_ref(&mut out, "");
        push_ref(
            &mut out,
            &format!("@{} ({}):", comment.author, comment.created_at),
        );
        push_ref(&mut out, &comment.body);
    }
    out
}

fn push_ref(out: &mut String, text: &str) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        out.push_str("#|");
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
}

/// Detect whether any new comment arrived on `live_thread` since `baseline_comment_ids`.
pub fn has_new_comments(baseline_comment_ids: &[String], live_thread: &ReviewThread) -> bool {
    let known: HashSet<&str> = baseline_comment_ids.iter().map(String::as_str).collect();
    live_thread
        .comments
        .iter()
        .any(|c| !known.contains(c.id.as_str()))
}

/// Validate a repository-relative path before opening it in Zed.
pub fn resolve_open_path(root: &Path, relative_path: &str) -> Result<PathBuf> {
    let rel = Path::new(relative_path);
    ensure!(
        !relative_path.is_empty() && !rel.is_absolute(),
        "Thread path must be relative: {relative_path}"
    );
    for component in rel.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("Unsafe path component in thread path: {relative_path}"),
        }
    }
    let canonical_root = root
        .canonicalize()
        .context("Cannot resolve workspace root")?;
    let full = canonical_root.join(rel);
    if full.exists() {
        let canonical = full
            .canonicalize()
            .with_context(|| format!("Cannot resolve {}", full.display()))?;
        ensure!(
            canonical.starts_with(&canonical_root),
            "Thread path escapes workspace root: {relative_path}"
        );
        return Ok(canonical);
    }
    Ok(full)
}

/// Open a thread's `file:line` in Zed, optionally checking out the matching jj revision.
pub fn open_in_zed(
    root: &Path,
    target_change_id: Option<&str>,
    relative_path: &str,
    line: Option<u32>,
    checkout: bool,
) -> Result<Option<String>> {
    let mut warning = None;
    if let Some(change_id) = target_change_id
        && !change_id.is_empty()
    {
        if checkout {
            let status = Command::new("jj")
                .current_dir(root)
                .args(["edit", change_id])
                .status()
                .context("Failed to run `jj edit`")?;
            ensure!(status.success(), "`jj edit {change_id}` failed ({status})");
        } else if let Ok(current) = current_change_id(root)
            && current != change_id
        {
            warning = Some(format!(
                "Note: jj @ is on {current}, while this thread belongs to {change_id}"
            ));
        }
    }

    let abs_path = resolve_open_path(root, relative_path)?;
    let target = match line {
        Some(l) if l > 0 => format!("{}:{l}", abs_path.display()),
        _ => abs_path.display().to_string(),
    };
    let status = Command::new("zed")
        .arg("--existing")
        .arg(&target)
        .status()
        .with_context(|| format!("Failed to launch `zed --existing {target}`"))?;
    ensure!(
        status.success(),
        "`zed --existing {target}` exited with {status}"
    );
    Ok(warning)
}

pub fn current_change_id(root: &Path) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(root)
        .args([
            "log",
            "--ignore-working-copy",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "change_id",
        ])
        .output()
        .context("Failed to read current jj change_id")?;
    ensure!(
        output.status.success(),
        "jj log failed while reading @ change_id"
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Check whether local bookmarks for target drafts differ from their PR's remote `head_sha`.
pub fn check_unpushed_bookmarks(
    client: &Client,
    drafts: &[FeedbackDraft],
    revset: Option<&str>,
) -> Result<Vec<UnpushedBookmark>> {
    let stack = push::discover_stack_bookmarks(&client.root, &client.git_dir, revset, false)?;
    let by_name: HashMap<&str, &StackBookmark> =
        stack.iter().map(|b| (b.bookmark.as_str(), b)).collect();
    let mut checked_prs: HashMap<u64, String> = HashMap::new();
    let mut unpushed = Vec::new();
    let mut seen_bookmarks = HashSet::new();

    for draft in drafts {
        if draft.bookmark.is_empty() || !seen_bookmarks.insert(draft.bookmark.clone()) {
            continue;
        }
        let Some(local) = by_name.get(draft.bookmark.as_str()) else {
            continue;
        };
        let remote_head = if let Some(sha) = checked_prs.get(&draft.pr_number) {
            sha.clone()
        } else {
            let pr = client.read_pr(draft.pr_number)?;
            checked_prs.insert(draft.pr_number, pr.head_sha.clone());
            pr.head_sha
        };
        if !local.commit_id.eq_ignore_ascii_case(&remote_head) {
            unpushed.push(UnpushedBookmark {
                bookmark: draft.bookmark.clone(),
                pr_number: draft.pr_number,
                local_commit_id: local.commit_id.clone(),
                remote_head_sha: remote_head,
            });
        }
    }
    Ok(unpushed)
}

/// Batch-publish all staged feedback drafts for this host/repo safely.
pub fn publish_staged(
    client: &Client,
    feedback_dir: &Path,
    revset: Option<&str>,
    allow_unpushed: bool,
    yes: bool,
    mut confirm_fn: impl FnMut(&str) -> Result<bool>,
) -> Result<PublishReport> {
    let all_drafts = list_all_drafts(feedback_dir)?;
    let mut targets: Vec<(PathBuf, FeedbackDraft)> = Vec::new();
    for (dir, draft) in all_drafts {
        if !draft.host.eq_ignore_ascii_case(&client.host)
            || !draft.repo.eq_ignore_ascii_case(&client.repo)
        {
            continue;
        }
        draft.editable()?;
        if draft.has_staged_work() {
            targets.push((dir, draft));
        }
    }

    if targets.is_empty() {
        println!("No staged feedback replies or resolutions to publish.");
        return Ok(PublishReport::default());
    }

    if !allow_unpushed {
        let draft_refs: Vec<FeedbackDraft> = targets.iter().map(|(_, d)| d.clone()).collect();
        let unpushed = check_unpushed_bookmarks(client, &draft_refs, revset)?;
        if !unpushed.is_empty() {
            let mut details = String::new();
            for item in &unpushed {
                details.push_str(&format!(
                    "\n  - {} (PR #{}): local {} != remote {}",
                    item.bookmark,
                    item.pr_number,
                    short_sha(&item.local_commit_id),
                    short_sha(&item.remote_head_sha)
                ));
            }
            bail!(
                "Local bookmarks differ from GitHub PR heads:{details}\nPush your stack first (`zed-pr-review push`) or re-run with `--allow-unpushed`."
            );
        }
    }

    let reply_count = targets
        .iter()
        .filter(|(_, d)| {
            d.reply_body
                .as_deref()
                .is_some_and(|b| !b.trim().is_empty())
        })
        .count();
    let resolve_count = targets.iter().filter(|(_, d)| d.resolve).count();

    println!(
        "Ready to publish {} repl{} and resolve {} thread{} across {} thread(s):",
        reply_count,
        if reply_count == 1 { "y" } else { "ies" },
        resolve_count,
        if resolve_count == 1 { "" } else { "s" },
        targets.len()
    );
    for (_, draft) in &targets {
        let loc = match draft.line {
            Some(line) => format!("{}:{line}", draft.path),
            None => draft.path.clone(),
        };
        let mut actions = Vec::new();
        if draft
            .reply_body
            .as_deref()
            .is_some_and(|b| !b.trim().is_empty())
        {
            actions.push("reply");
        }
        if draft.resolve {
            actions.push("resolve");
        }
        println!(
            "  - PR #{} ({}) {} [{}]",
            draft.pr_number,
            draft.bookmark,
            loc,
            actions.join(" + ")
        );
    }

    if !yes && !confirm_fn("Publish staged feedback to GitHub? Type yes: ")? {
        println!("Cancelled. All staged feedback kept locally.");
        return Ok(PublishReport::default());
    }

    let mut report = PublishReport::default();
    for (dir, _) in targets {
        let store = Store::at(&dir)?;
        let Some(mut draft) = load_draft(&store)? else {
            continue;
        };
        draft.editable()?;
        if !draft.has_staged_work() {
            continue;
        }

        let live =
            client.fetch_review_thread(&draft.thread_id, draft.pr_number, &draft.bookmark, "")?;

        if has_new_comments(&draft.baseline_comment_ids, &live) {
            let loc = match draft.line {
                Some(line) => format!("{}:{line}", draft.path),
                None => draft.path.clone(),
            };
            eprintln!(
                "Warning: skipping PR #{} {} — new comment(s) arrived since drafting. Inspect and re-stage before publishing.",
                draft.pr_number, loc
            );
            report
                .skipped_stale
                .push((draft.pr_number, draft.thread_id.clone(), loc));
            continue;
        }

        let reply_body = draft
            .reply_body
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(|b| {
                if draft.reply_agent {
                    crate::message::format_agent_body(b)
                } else {
                    Ok(b.to_owned())
                }
            })
            .transpose()?;
        let want_resolve = draft.resolve && !live.is_resolved;
        if draft.resolve && live.is_resolved {
            report.skipped_already_resolved += 1;
        }

        let mut attempt = FeedbackAttempt {
            reply_body: reply_body.clone(),
            resolve: want_resolve,
            previous_comment_ids: live.comment_ids(),
            reply_posted: None,
            resolved_confirmed: false,
        };
        draft.attempt = Some(attempt.clone());
        save_draft(&store, &draft)?;

        let mut reply_receipt = None;
        if let Some(body) = &reply_body {
            let posted = client
                .reply_to_review_thread(&draft.thread_id, body)
                .with_context(|| {
                    format!(
                        "Failed while posting reply to PR #{} ({}). Attempt preserved; run `zed-pr-review feedback recover`",
                        draft.pr_number, draft.path
                    )
                })?;
            let receipt = ReplyReceipt {
                comment_id: posted.id.clone(),
                url: posted.url.clone(),
            };
            attempt.reply_posted = Some(receipt.clone());
            draft.attempt = Some(attempt.clone());
            save_draft(&store, &draft).context(
                "Reply was posted to GitHub, but checkpointing attempt failed. Run `zed-pr-review feedback recover`",
            )?;
            reply_receipt = Some(receipt);
            report.published_replies += 1;
        }

        let mut resolved = false;
        if want_resolve {
            let ok = client
                .resolve_review_thread(&draft.thread_id)
                .with_context(|| {
                    format!(
                        "Failed while resolving thread on PR #{} ({}). Attempt preserved; run `zed-pr-review feedback recover`",
                        draft.pr_number, draft.path
                    )
                })?;
            ensure!(
                ok,
                "GitHub returned isResolved=false for PR #{} ({}); run `zed-pr-review feedback recover`",
                draft.pr_number,
                draft.path
            );
            attempt.resolved_confirmed = true;
            resolved = true;
            report.resolved_threads += 1;
        }

        let mut updated_baseline = live.comment_ids();
        if let Some(ref r) = reply_receipt {
            updated_baseline.push(r.comment_id.clone());
        }
        draft.baseline_comment_ids = updated_baseline;
        draft.submitted = Some(FeedbackReceipt {
            reply: reply_receipt,
            resolved,
        });
        draft.attempt = None;
        draft.reply_body = None;
        draft.reply_agent = false;
        draft.resolve = false;
        save_draft(&store, &draft).context(
            "GitHub accepted the feedback mutation, but saving receipt failed. Run `zed-pr-review feedback recover`",
        )?;
    }

    println!(
        "Done: {} repl{} posted, {} thread{} resolved{}.",
        report.published_replies,
        if report.published_replies == 1 {
            "y"
        } else {
            "ies"
        },
        report.resolved_threads,
        if report.resolved_threads == 1 {
            ""
        } else {
            "s"
        },
        if report.skipped_stale.is_empty() {
            String::new()
        } else {
            format!(
                ", {} skipped due to new comments",
                report.skipped_stale.len()
            )
        }
    );
    Ok(report)
}

/// Reconcile uncertain feedback publish attempts against GitHub.
pub fn recover_attempts(
    client: &Client,
    feedback_dir: &Path,
    retry: bool,
    mut confirm_fn: impl FnMut(&str) -> Result<bool>,
) -> Result<RecoverReport> {
    let all_drafts = list_all_drafts(feedback_dir)?;
    let mut report = RecoverReport::default();

    for (dir, _) in all_drafts {
        let store = Store::at(&dir)?;
        let Some(mut draft) = load_draft(&store)? else {
            continue;
        };
        if !draft.host.eq_ignore_ascii_case(&client.host)
            || !draft.repo.eq_ignore_ascii_case(&client.repo)
        {
            continue;
        }
        let Some(mut attempt) = draft.attempt.clone() else {
            continue;
        };

        let live =
            client.fetch_review_thread(&draft.thread_id, draft.pr_number, &draft.bookmark, "")?;

        // Check if reply landed on GitHub.
        if attempt.reply_posted.is_none()
            && let Some(expected_body) = attempt.reply_body.as_deref()
            && let Some(found) = find_matching_reply(
                &live.comments,
                &attempt.previous_comment_ids,
                &client.reviewer,
                expected_body,
            )
        {
            attempt.reply_posted = Some(ReplyReceipt {
                comment_id: found.id.clone(),
                url: found.url.clone(),
            });
        }

        let reply_done = attempt.reply_body.is_none() || attempt.reply_posted.is_some();
        let resolve_done = !attempt.resolve || live.is_resolved;

        if reply_done && resolve_done {
            draft.submitted = Some(FeedbackReceipt {
                reply: attempt.reply_posted.clone(),
                resolved: attempt.resolve && live.is_resolved,
            });
            draft.baseline_comment_ids = live.comment_ids();
            draft.attempt = None;
            draft.reply_body = None;
            draft.resolve = false;
            save_draft(&store, &draft)?;
            println!(
                "Recovered PR #{} ({}): already landed on GitHub.",
                draft.pr_number, draft.path
            );
            report.recovered += 1;
            continue;
        }

        // Persist any newly discovered partial progress (e.g. reply landed, resolve didn't).
        if attempt.reply_posted.is_some() {
            draft.reply_body = None;
            draft.baseline_comment_ids = live.comment_ids();
        }
        draft.attempt = Some(attempt.clone());
        save_draft(&store, &draft)?;

        if !retry {
            println!(
                "PR #{} ({}): attempt still uncertain (reply_landed={}, resolved={}). Check GitHub, then run `zed-pr-review feedback recover --retry` to re-enable.",
                draft.pr_number,
                draft.path,
                attempt.reply_posted.is_some(),
                live.is_resolved
            );
            report.pending_retry += 1;
            continue;
        }

        let prompt = format!(
            "Confirm on GitHub that remaining action(s) for PR #{} ({}) did NOT land. Type yes to enable retry: ",
            draft.pr_number, draft.path
        );
        if confirm_fn(&prompt)? {
            let archive = archive_draft(&store, &draft)?;
            if let Some(old_attempt) = draft.attempt.take() {
                draft.retry_history.push(old_attempt);
            }
            save_draft(&store, &draft)?;
            println!(
                "Retry enabled for PR #{} ({}); backup at {}.",
                draft.pr_number,
                draft.path,
                archive.display()
            );
            report.retry_enabled += 1;
        } else {
            println!(
                "Cancelled; uncertain attempt kept for PR #{} ({}).",
                draft.pr_number, draft.path
            );
            report.pending_retry += 1;
        }
    }

    if report.recovered == 0 && report.pending_retry == 0 && report.retry_enabled == 0 {
        println!("No uncertain feedback publish attempts found.");
    }
    Ok(report)
}

fn find_matching_reply<'a>(
    comments: &'a [ThreadComment],
    previous_ids: &[String],
    author: &str,
    expected_body: &str,
) -> Option<&'a ThreadComment> {
    let prev: HashSet<&str> = previous_ids.iter().map(String::as_str).collect();
    comments.iter().find(|c| {
        !prev.contains(c.id.as_str())
            && c.author.eq_ignore_ascii_case(author)
            && c.body.trim() == expected_body.trim()
    })
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Summary of staged feedback for `ctrl-shift-x` menu display.
#[derive(Clone, Debug, Default)]
pub struct StagedFeedbackSummary {
    pub staged_replies: usize,
    pub staged_resolves: usize,
    pub staged_threads: usize,
    pub uncertain_attempts: usize,
}

pub fn summarize_staged(feedback_dir: &Path, host: &str, repo: &str) -> StagedFeedbackSummary {
    let mut summary = StagedFeedbackSummary::default();
    let Ok(drafts) = list_all_drafts(feedback_dir) else {
        return summary;
    };
    for (_, draft) in drafts {
        if !draft.host.eq_ignore_ascii_case(host) || !draft.repo.eq_ignore_ascii_case(repo) {
            continue;
        }
        if draft.attempt.is_some() {
            summary.uncertain_attempts += 1;
        }
        if draft.has_staged_work() {
            summary.staged_threads += 1;
            if draft
                .reply_body
                .as_deref()
                .is_some_and(|b| !b.trim().is_empty())
            {
                summary.staged_replies += 1;
            }
            if draft.resolve {
                summary.staged_resolves += 1;
            }
        }
    }
    summary
}

// ---------------------------------------------------------------------------
// Background unresolved-thread badge worker for `stack --watch`
// ---------------------------------------------------------------------------

pub enum ThreadBadgeRequest {
    UpdateCommits(Vec<String>),
}

pub struct ThreadBadgeHandle {
    pub requests: mpsc::Sender<ThreadBadgeRequest>,
    pub replies: mpsc::Receiver<HashMap<String, usize>>,
}

/// Spawn a background worker that polls unresolved GitHub review thread counts
/// for your own bookmarked revisions visible in `jj l`. Never blocks `jj l`.
pub fn spawn_badge_worker(root: &Path) -> Option<ThreadBadgeHandle> {
    let root = root.to_owned();
    let (requests, jobs) = mpsc::channel();
    let (results, replies) = mpsc::channel();
    std::thread::Builder::new()
        .name("thread-badges".into())
        .spawn(move || run_badge_worker(&root, jobs, results))
        .ok()?;
    Some(ThreadBadgeHandle { requests, replies })
}

fn run_badge_worker(
    root: &Path,
    jobs: mpsc::Receiver<ThreadBadgeRequest>,
    results: mpsc::Sender<HashMap<String, usize>>,
) {
    let mut client: Option<Client> = None;
    let mut visible_commits: HashSet<String> = HashSet::new();
    let mut last_polled_bookmarks: BTreeMap<String, String> = BTreeMap::new();
    let mut counts_by_bookmark: HashMap<String, usize> = HashMap::new();
    let mut last_sent: HashMap<String, usize> = HashMap::new();
    let mut last_poll: Option<Instant> = None;

    loop {
        let timeout = match last_poll {
            None => BADGE_POLL_INTERVAL,
            Some(last) => BADGE_POLL_INTERVAL.saturating_sub(last.elapsed()),
        };
        let mut updated = false;
        match jobs.recv_timeout(timeout) {
            Ok(ThreadBadgeRequest::UpdateCommits(commits)) => {
                visible_commits = commits.into_iter().collect();
                updated = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(ThreadBadgeRequest::UpdateCommits(commits)) = jobs.try_recv() {
            visible_commits = commits.into_iter().collect();
            updated = true;
        }

        if visible_commits.is_empty() {
            continue;
        }

        if client.is_none() {
            client = github::with_background_deadlines(|| Client::for_workspace(root, None)).ok();
        }
        let Some(ref gh) = client else {
            continue;
        };

        let Ok(pushed_bms) = push::local_pushed_bookmarks(root, &gh.git_dir) else {
            continue;
        };
        let visible_bms: BTreeMap<String, String> = pushed_bms
            .into_iter()
            .filter(|(_, commit_id)| visible_commits.contains(commit_id))
            .collect();

        if updated {
            let mapped = map_counts_to_commits(&visible_bms, &counts_by_bookmark);
            if mapped != last_sent {
                if results.send(mapped.clone()).is_err() {
                    break;
                }
                last_sent = mapped;
            }
        }

        let due = last_poll.is_none_or(|t| t.elapsed() >= BADGE_POLL_INTERVAL);
        let bms_changed = visible_bms != last_polled_bookmarks;
        if !due && !bms_changed {
            continue;
        }

        last_poll = Some(Instant::now());
        last_polled_bookmarks = visible_bms.clone();

        if visible_bms.is_empty() {
            if !last_sent.is_empty() {
                if results.send(HashMap::new()).is_err() {
                    break;
                }
                last_sent.clear();
            }
            continue;
        }

        let mut next_counts = HashMap::new();
        for bookmark in visible_bms.keys() {
            let count =
                github::with_background_deadlines(|| fetch_own_pr_unresolved_count(gh, bookmark));
            if let Ok(Some(n)) = count
                && n > 0
            {
                next_counts.insert(bookmark.clone(), n);
            }
        }
        counts_by_bookmark = next_counts;
        let mapped = map_counts_to_commits(&visible_bms, &counts_by_bookmark);
        if mapped != last_sent {
            if results.send(mapped.clone()).is_err() {
                break;
            }
            last_sent = mapped;
        }
    }
}

fn map_counts_to_commits(
    visible_bms: &BTreeMap<String, String>,
    counts_by_bookmark: &HashMap<String, usize>,
) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for (bm, commit_id) in visible_bms {
        if let Some(&count) = counts_by_bookmark.get(bm)
            && count > 0
        {
            *out.entry(commit_id.clone()).or_insert(0) += count;
        }
    }
    out
}

fn fetch_own_pr_unresolved_count(client: &Client, bookmark: &str) -> Result<Option<usize>> {
    let prs = client.list_prs_for_head(bookmark)?;
    let Some(pr) = prs.into_iter().find(|p| {
        p.state == BranchPrState::Open && p.author.eq_ignore_ascii_case(&client.reviewer)
    }) else {
        return Ok(None);
    };
    let (owner, name) = client
        .repo
        .split_once('/')
        .context("Repository must be OWNER/REPO")?;
    let response = client.graphql(
        "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviewThreads(first: 100) { nodes { isResolved } } } } }",
        &json!({ "owner": owner, "name": name, "number": pr.number }),
    )?;
    let Some(nodes) = response
        .get("data")
        .and_then(|d| d.get("repository"))
        .and_then(|r| r.get("pullRequest"))
        .and_then(|p| p.get("reviewThreads"))
        .and_then(|t| t.get("nodes"))
        .and_then(|n| n.as_array())
    else {
        return Ok(None);
    };
    let unresolved = nodes
        .iter()
        .filter(|n| n.get("isResolved").and_then(|v| v.as_bool()) == Some(false))
        .count();
    Ok(Some(unresolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::review_summary;
    use tempfile::TempDir;

    fn sample_thread(id: &str, comment_ids: &[&str]) -> ReviewThread {
        ReviewThread {
            id: id.into(),
            is_resolved: false,
            is_outdated: false,
            is_collapsed: false,
            path: "src/lib.rs".into(),
            line: Some(12),
            original_line: Some(12),
            start_line: None,
            diff_side: "RIGHT".into(),
            comments: comment_ids
                .iter()
                .enumerate()
                .map(|(i, cid)| ThreadComment {
                    id: (*cid).into(),
                    database_id: Some(100 + i as u64),
                    author: "reviewer".into(),
                    body: format!("Comment {cid}"),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    url: format!("https://github.com/org/repo/pull/1#discussion_r{}", 100 + i),
                })
                .collect(),
            pr_number: 7,
            bookmark: "feat-a".into(),
            head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        }
    }

    #[test]
    fn thread_draft_key_normalizes_host_and_repo_and_isolates_threads() {
        let a = thread_draft_key("GitHub.COM", "Org/Repo", 7, "PRRT_1");
        let b = thread_draft_key("github.com", "org/repo", 7, "PRRT_1");
        let c = thread_draft_key("github.com", "org/repo", 7, "PRRT_2");
        let d = thread_draft_key("github.com", "org/repo", 8, "PRRT_1");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn draft_storage_locking_and_round_trip() {
        let temp = TempDir::new().unwrap();
        let fb = feedback_root(temp.path());
        let thread = sample_thread("PRRT_1", &["C1"]);

        let draft = stage_reply(&fb, "github.com", "org/repo", &thread, "Fixed!", false).unwrap();
        assert_eq!(draft.reply_body.as_deref(), Some("Fixed!"));
        assert!(!draft.reply_agent);
        assert!(!draft.resolve);
        assert_eq!(draft.baseline_comment_ids, vec!["C1"]);

        let updated = set_resolve(&fb, "github.com", "org/repo", &thread, true)
            .unwrap()
            .unwrap();
        assert!(updated.resolve);
        assert_eq!(updated.reply_body.as_deref(), Some("Fixed!"));

        // Holding a lock prevents concurrent edits on the same thread.
        let _lock = open_thread_store(&fb, "github.com", "org/repo", 7, "PRRT_1").unwrap();
        assert!(stage_reply(&fb, "github.com", "org/repo", &thread, "Other", false).is_err());
        drop(_lock);

        // Agent flag persists locally without quoting the stored body.
        let agent_draft = stage_reply(
            &fb,
            "github.com",
            "org/repo",
            &thread,
            "Raw agent reply.",
            true,
        )
        .unwrap();
        assert_eq!(agent_draft.reply_body.as_deref(), Some("Raw agent reply."));
        assert!(agent_draft.reply_agent);

        assert!(clear_staged(&fb, "github.com", "org/repo", &thread).unwrap());
        let all = list_all_drafts(&fb).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn stale_thread_detection_catches_new_comments() {
        let baseline = vec!["C1".to_owned(), "C2".to_owned()];
        let same = sample_thread("PRRT_1", &["C1", "C2"]);
        let newer = sample_thread("PRRT_1", &["C1", "C2", "C3"]);
        assert!(!has_new_comments(&baseline, &same));
        assert!(has_new_comments(&baseline, &newer));
    }

    #[test]
    fn visible_threads_includes_outdated_unresolved_by_default() {
        let mut outdated = sample_thread("PRRT_1", &["C1"]);
        outdated.is_outdated = true;
        let mut resolved = sample_thread("PRRT_2", &["C2"]);
        resolved.is_resolved = true;
        let index = FeedbackIndex {
            version: INDEX_VERSION,
            host: "github.com".into(),
            repo: "org/repo".into(),
            groups: vec![IndexedGroup {
                bookmark: "feat-a".into(),
                change_id: "chg1".into(),
                commit_id: "com1".into(),
                pr_number: 7,
                pr_title: "Feat A".into(),
                pr_url: "https://github.com/org/repo/pull/7".into(),
                head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                threads: vec![outdated, resolved],
            }],
        };
        let default_visible = visible_threads(&index, false);
        assert_eq!(default_visible.len(), 1);
        assert_eq!(default_visible[0].thread.id, "PRRT_1");

        let all_visible = visible_threads(&index, true);
        assert_eq!(all_visible.len(), 2);
    }

    #[test]
    fn reply_template_strips_reference_lines_cleanly() {
        let thread = sample_thread("PRRT_1", &["C1"]);
        let template = reply_template(&thread, None);
        assert!(review_summary(&template).trim().is_empty());
        let with_reply = format!("Done in latest commit.\n{template}");
        assert_eq!(review_summary(&with_reply).trim(), "Done in latest commit.");
    }

    #[test]
    fn resolve_open_path_rejects_traversal_and_absolute_paths() {
        let temp = TempDir::new().unwrap();
        assert!(resolve_open_path(temp.path(), "../secret").is_err());
        assert!(resolve_open_path(temp.path(), "/etc/passwd").is_err());
        assert!(resolve_open_path(temp.path(), "").is_err());
        let ok = resolve_open_path(temp.path(), "src/lib.rs").unwrap();
        assert!(ok.ends_with("src/lib.rs"));
    }

    #[test]
    fn unrecovered_attempt_blocks_editing() {
        let mut draft =
            FeedbackDraft::from_thread("github.com", "org/repo", &sample_thread("PRRT_1", &["C1"]));
        draft.attempt = Some(FeedbackAttempt {
            reply_body: Some("hi".into()),
            resolve: true,
            previous_comment_ids: vec!["C1".into()],
            reply_posted: None,
            resolved_confirmed: false,
        });
        assert!(draft.editable().is_err());
    }
}
