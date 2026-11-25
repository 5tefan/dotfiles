//! Frozen, full-PR directory diffs. Snapshot paths are capabilities, not hints:
//! only an exact, registered RHS file may be used to make a review comment.

use crate::github::Client;
use crate::model::{ChangedFile, PullRequest};
use crate::selection::repo_path;
use crate::store::{Draft, Receipt, Store, draft_key};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: u32 = 1;
const STATE_PATH: &str = "scratch/zed-pr-review";
const CLEANUP_FILE: &str = "cleanup.json";
const CANCELLED_FILE: &str = "cancelled.json";
const PREPARED_FILE: &str = "prepared.json";
const STAGING_PREFIX: &str = "staging-";

#[derive(Debug)]
pub struct Session {
    pub pr: PullRequest,
    pub root: PathBuf,
    pub directory: PathBuf,
    pub merge_base: String,
    manifest: Manifest,
    manifest_sha256: String,
    cleanup: Option<Cleanup>,
    cancelled: Option<Cancelled>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    root: PathBuf,
    directory: PathBuf,
    pr: PullRequest,
    merge_base: String,
    // Old manifests recorded only has_patch, not the GitHub patch contents.
    #[serde(default)]
    patches_cached: bool,
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    filename: String,
    status: String,
    previous_filename: Option<String>,
    has_patch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
    binary: bool,
    base: Option<Snapshot>,
    head: Option<Snapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    // Relative to the session directory, including the base/ or head/ prefix.
    path: String,
    repository_path: String,
    sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Active {
    version: u32,
    root: PathBuf,
    directory: PathBuf,
    manifest_sha256: String,
}

// This immutable receipt permits missing snapshots, but never changes their
// registered paths or the identity of the session. It survives draft archival.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cleanup {
    version: u32,
    manifest_sha256: String,
    review_id: u64,
    review_state: String,
    review_url: String,
}

// Unlike Cleanup, this marker records only a local decision. Its draft digest
// permits retries to reuse the archive published before any cancellation marker.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cancelled {
    version: u32,
    manifest_sha256: String,
    draft_sha256: String,
    // Manifest hashes include the immutable session directory, so reopening the
    // same PR revision cannot join an earlier cancellation by accident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sessions: Option<BTreeSet<String>>,
}

impl Session {
    /// Read pinned local blobs without fetching, checking out, or requiring jj @
    /// to match. Before commenting, the caller must verify the current PR and
    /// require jj @ to be its pinned head or an unchanged single-parent child.
    pub fn prepare(client: &Client, store: &Store, pr: &PullRequest) -> Result<Self> {
        let files = client.files(pr.number)?;
        let session = Self::prepare_local(client, store, pr, &files)?;
        ensure!(
            pr.same_revision(&client.read_pr(pr.number)?),
            "PR changed while preparing the full diff; prepare a new session"
        );
        Ok(session)
    }

    /// Build from already-fetched metadata and objects without network calls.
    /// The caller must recheck PR pins after reading `files`, before calling this.
    /// Background preparation uses this to keep network waits outside draft locks.
    pub fn prepare_local(
        client: &Client,
        store: &Store,
        pr: &PullRequest,
        files: &[ChangedFile],
    ) -> Result<Self> {
        let root = workspace_root(&client.root)?;
        let merge_base = client.merge_base(pr)?;
        create_session(&root, store, pr, &merge_base, files, |sha, path| {
            client.blob(sha, path)
        })
    }

    pub fn head_root(&self) -> PathBuf {
        self.directory.join("head")
    }

    /// Revalidate both the manifest and the complete snapshot before returning
    /// a repository filename. Never resolve an external file by its contents.
    pub fn selection_path(&self, file: &Path) -> Result<String> {
        self.validate()?;
        Ok(self.checked_selection(file)?.repository_path.clone())
    }

    // The caller already validated the complete session. Keep exact RHS mapping
    // checks here so load_for_file does not load and hash every snapshot twice.
    fn checked_selection(&self, file: &Path) -> Result<&Snapshot> {
        self.require_live()?;
        check_path(&self.root, file, false)?;
        // Keep the existing selection validator, including its no-symlink rule.
        let relative = repo_path(&self.head_root(), file)?;
        let snapshot = self
            .manifest
            .files
            .iter()
            .filter_map(|entry| entry.head.as_ref())
            .find(|snapshot| snapshot.path == format!("head/{relative}"))
            .context("Selected file is not an exact registered RHS snapshot")?;
        ensure!(
            relative == snapshot.repository_path,
            "RHS snapshot does not mirror its repository filename"
        );
        Ok(snapshot)
    }

    /// Cached GitHub metadata, including files whose patch is absent or empty.
    /// None means a legacy manifest: the caller must obtain patches elsewhere.
    /// This reads the loaded manifest only; it does not revalidate the session.
    pub fn comment_files(&self) -> Option<Vec<ChangedFile>> {
        self.manifest.patches_cached.then(|| {
            self.manifest
                .files
                .iter()
                .map(|file| ChangedFile {
                    filename: file.filename.clone(),
                    status: file.status.clone(),
                    previous_filename: file.previous_filename.clone(),
                    patch: file.patch.clone(),
                })
                .collect()
        })
    }

    /// Local data for a comment prompt: repository path, pinned head bytes, and
    /// cached GitHub files (None for legacy manifests).
    ///
    /// The caller must have just fully loaded this session with load_for_file,
    /// or called validate. This checks the exact RHS mapping, selected bytes'
    /// digest, immutable manifest, and live markers without rescanning unrelated
    /// snapshots. After user input, use selection_path/validate again. This is
    /// not a replacement for full validation before saving or submitting.
    pub fn comment_data(&self, file: &Path) -> Result<(String, Vec<u8>, Option<Vec<ChangedFile>>)> {
        self.check_identity()?;
        let snapshot = self.checked_selection(file)?;
        let bytes = fs::read(file).context("Cannot read selected head snapshot")?;
        ensure!(
            digest(&bytes) == snapshot.sha256,
            "Snapshot content changed: {}; prepare a new diff",
            file.display()
        );
        let manifest_path = self.directory.join("manifest.json");
        check_path(&self.root, &manifest_path, false)?;
        ensure!(
            digest(&fs::read(manifest_path)?) == self.manifest_sha256,
            "Session manifest changed; prepare a new diff instead of reusing these snapshots"
        );
        ensure!(
            !exists_without_following(&self.directory.join(CANCELLED_FILE))?,
            "This local review was cancelled; reopen the PR for a new review"
        );
        ensure!(
            !exists_without_following(&self.directory.join(CLEANUP_FILE))?,
            "Review snapshots were cleaned after submission; reopen the PR to prepare a new diff"
        );
        Ok((
            snapshot.repository_path.clone(),
            bytes,
            self.comment_files(),
        ))
    }

    /// Remember a fully validated session for this PR/reviewer without changing
    /// the active session or draft. Call only after a successful prepare/open.
    pub fn remember(&self, store: &Store) -> Result<()> {
        self.validate()?;
        ensure!(
            exact_path(
                &store.directory,
                &self.root.join(STATE_PATH).join(draft_key(&self.pr))
            ),
            "Prepared session requires the exact local store for this PR and reviewer"
        );
        check_path(&self.root, &store.directory, true)?;
        let path = store.directory.join(PREPARED_FILE);
        if exists_without_following(&path)? {
            check_path(&self.root, &path, false)?;
        }
        store.write_json(
            PREPARED_FILE,
            &Active {
                version: VERSION,
                root: self.root.clone(),
                directory: self.directory.clone(),
                manifest_sha256: self.manifest_sha256.clone(),
            },
        )
    }

    /// Reuse only an intact, live session for exactly the requested revision.
    /// This local cache is disposable: missing, stale, or corrupt state is a
    /// miss, never a foreground error. No pointer, draft, or session is changed.
    pub fn prepared(store: &Store, pr: &PullRequest) -> Result<Option<Self>> {
        Ok(load_prepared(store, pr).ok())
    }

    /// Atomically replace only the active pointer. Existing diff directories
    /// and the sessions associated with old Zed tabs are never reused.
    pub fn activate(&self, state_base: &Path) -> Result<()> {
        self.validate()?;
        check_state_base(&self.root, state_base)?;
        check_path(&self.root, state_base, true)?;
        let path = state_base.join("active.json");
        if exists_without_following(&path)? {
            check_path(&self.root, &path, false)?;
        }
        let active = Active {
            version: VERSION,
            root: self.root.clone(),
            directory: self.directory.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
        };
        let mut temporary = tempfile::NamedTempFile::new_in(state_base)?;
        serde_json::to_writer_pretty(&mut temporary, &active)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(state_base)?.sync_all()?;
        Ok(())
    }

    pub fn open(&self) -> Result<()> {
        self.validate()?;
        println!("Full PR diff: {} → {}", self.merge_base, self.pr.head_sha);
        println!("Frozen snapshots: {}", self.directory.display());
        println!(
            "Select only RHS (head) text when commenting. Zed task context has no reliable side flag; a reported RHS path does not prove that the selection came from the RHS."
        );
        println!(
            "Renames may appear as removal/addition pairs; pure renames and mode-only changes may have no text diff. Binary files and files without a GitHub patch are view-only."
        );
        for file in &self.manifest.files {
            let rename = file
                .previous_filename
                .as_ref()
                .map(|old| format!(" (from {old})"))
                .unwrap_or_default();
            let caveat = if file.binary {
                " [binary/non-UTF-8: view-only]"
            } else if !file.has_patch {
                " [no GitHub patch: view-only]"
            } else if file.head.is_none() {
                " [deleted: no RHS selection]"
            } else {
                ""
            };
            println!("{}: {}{rename}{caveat}", file.status, file.filename);
        }
        if self.manifest.files.is_empty() {
            println!("The full PR diff is empty; there are no changed files to display.");
            return Ok(());
        }
        let status = Command::new("zed")
            .current_dir(&self.root)
            .arg("--existing")
            .arg("--diff")
            .arg(self.directory.join("base"))
            .arg(self.head_root())
            .status()
            .context("Cannot open the full PR directory diff in Zed")?;
        ensure!(status.success(), "Zed could not open the full PR diff");
        Ok(())
    }

    /// Require intact, live snapshots; retained cleanup metadata is not enough
    /// to authorize commenting or a new review decision.
    pub fn validate(&self) -> Result<()> {
        self.check_identity()?;
        let current = load_directory(&self.root, &self.root.join(STATE_PATH), &self.directory)?;
        ensure!(
            current.manifest_sha256 == self.manifest_sha256,
            "Session manifest changed; prepare a new diff instead of reusing these snapshots"
        );
        current.require_live()
    }

    fn check_identity(&self) -> Result<()> {
        ensure!(
            self.pr == self.manifest.pr
                && exact_path(&self.root, &self.manifest.root)
                && exact_path(&self.directory, &self.manifest.directory)
                && self.merge_base == self.manifest.merge_base,
            "Session identity was altered"
        );
        Ok(())
    }

    fn require_live(&self) -> Result<()> {
        self.require_not_cancelled()?;
        ensure!(
            self.cleanup.is_none(),
            "Review snapshots were cleaned after submission; reopen the PR to prepare a new diff"
        );
        Ok(())
    }

    fn require_not_cancelled(&self) -> Result<()> {
        ensure!(
            self.cancelled.is_none(),
            "This local review was cancelled; reopen the PR for a new review, or use cancel to retry local cleanup"
        );
        Ok(())
    }
}

/// Return None only for a file outside the review state. Files in review state
/// must belong to exactly one immutable session and be registered on its RHS.
pub fn load_for_file(root: &Path, state_base: &Path, file: &Path) -> Result<Option<Session>> {
    load_for_snapshot(root, state_base, file, false, false)
}

/// Load immutable context from either side, including removed snapshots after
/// cleanup. Also suitable for non-comment commands; comment coordinates remain
/// RHS-only. The caller must still check the current PR revision and require jj @
/// to be its pinned head or an unchanged single-parent child.
pub fn load_for_navigation(root: &Path, state_base: &Path, file: &Path) -> Result<Option<Session>> {
    load_for_snapshot(root, state_base, file, true, false)
}

/// Load only the exact registered head/base context, even after cancellation
/// removed its snapshots. This does not authorize comments, submission, or
/// navigation: those paths must use the ordinary loaders and Session::validate.
/// Recheck under workspace and Store locks before calling cancel_review: a
/// cancelled tab may retry only while its original session set and draft remain.
pub fn load_for_cancel(root: &Path, state_base: &Path, file: &Path) -> Result<Option<Session>> {
    let Some(session) = load_for_snapshot(root, state_base, file, true, true)? else {
        return Ok(None);
    };
    let store = session.directory.parent().context("Session has no store")?;
    let sessions = load_store_sessions(&session.root, state_base, store)?;
    if let Some(marker) = &session.cancelled {
        check_cancellation_context(&session.root, store, marker, &sessions)?;
    } else {
        // A crash can leave some of the original sessions unmarked. They may
        // retry that cancellation, but must not expand it to a fresh session.
        for marker in sessions
            .iter()
            .filter_map(|session| session.cancelled.as_ref())
        {
            if marker
                .sessions
                .as_ref()
                .is_some_and(|set| set.contains(&session.manifest_sha256))
            {
                check_cancellation_context(&session.root, store, marker, &sessions)?;
            }
        }
    }
    Ok(Some(session))
}

fn check_cancellation_context(
    root: &Path,
    store: &Path,
    marker: &Cancelled,
    sessions: &[Session],
) -> Result<()> {
    let original_sessions = match &marker.sessions {
        Some(original) => *original == session_hashes(sessions),
        // Older markers lack an exact scope. Never infer that an unmarked
        // session belongs to their interrupted cancellation.
        None => sessions.iter().all(|session| session.cancelled.is_some()),
    };
    ensure!(
        original_sessions,
        "This tab belongs to an earlier cancelled review; its session set changed. Use the current review tab instead; no local state was changed"
    );
    let path = store.join("draft.json");
    if exists_without_following(&path)? {
        check_path(root, &path, false)?;
        let draft: Draft =
            serde_json::from_slice(&fs::read(&path)?).context("Invalid persisted draft")?;
        ensure!(
            digest(&serde_json::to_vec(&draft)?) == marker.draft_sha256,
            "The draft changed after this review was cancelled; this old tab cannot abandon it. Use the current review tab instead"
        );
    }
    Ok(())
}

fn load_store_sessions(root: &Path, state_base: &Path, store: &Path) -> Result<Vec<Session>> {
    check_path(root, store, true)?;
    let mut sessions = Vec::new();
    for entry in fs::read_dir(store)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("diff-") {
            sessions.push(load_directory(root, state_base, &entry.path())?);
        }
    }
    Ok(sessions)
}

fn session_hashes(sessions: &[Session]) -> BTreeSet<String> {
    sessions
        .iter()
        .map(|session| session.manifest_sha256.clone())
        .collect()
}

fn load_for_snapshot(
    root: &Path,
    state_base: &Path,
    file: &Path,
    allow_base: bool,
    allow_cancelled: bool,
) -> Result<Option<Session>> {
    let root = workspace_root(root)?;
    check_state_base(&root, state_base)?;
    let lexical_member = file.starts_with(state_base);
    // Removed snapshots can still have an existing ancestor reached through an
    // alias. Such paths must fail closed rather than fall back to ordinary jj.
    let mut resolved_member = false;
    for ancestor in file.ancestors() {
        if let Ok(path) = ancestor.canonicalize() {
            resolved_member = path.starts_with(state_base);
            break;
        }
        ensure!(
            !exists_without_following(ancestor)?,
            "Selected context is dangling or cannot be resolved; use an exact registered snapshot path"
        );
    }
    if !lexical_member && !resolved_member {
        return Ok(None);
    }
    let relative = file
        .strip_prefix(state_base)
        .context("Selected state file uses an alias outside review state")?;
    let relative = relative.to_str().context("Snapshot path must be UTF-8")?;
    safe_relative(relative)?;
    let components: Vec<_> = relative.split('/').collect();
    ensure!(
        components.len() >= 4
            && (components[2] == "head" || (allow_base && components[2] == "base")),
        "Select an exact registered {}snapshot, not a prompt, manifest, or other state file",
        if allow_base { "diff " } else { "RHS " }
    );
    let directory = state_base.join(components[0]).join(components[1]);
    let session = load_directory(&root, state_base, &directory)?;
    ensure!(
        session.manifest.files.iter().any(|entry| {
            let snapshot = if components[2] == "head" {
                &entry.head
            } else {
                &entry.base
            };
            snapshot
                .as_ref()
                .is_some_and(|snapshot| exact_path(file, &session.directory.join(&snapshot.path)))
        }),
        "Selected file is not an exact registered diff snapshot"
    );
    if !allow_cancelled {
        session.require_not_cancelled()?;
    }
    if !allow_base {
        session.checked_selection(file)?;
    } else if session.cleanup.is_none() && session.cancelled.is_none() {
        check_path(&root, file, false)?;
    }
    Ok(Some(session))
}

fn load_prepared(store: &Store, pr: &PullRequest) -> Result<Session> {
    // Derive the workspace from the locked store, never from the cache pointer.
    let root = store
        .directory
        .ancestors()
        .nth(3)
        .context("Prepared session store has no workspace root")?;
    let state_base = root.join(STATE_PATH);
    check_state_base(root, &state_base)?;
    ensure!(
        exact_path(&store.directory, &state_base.join(draft_key(pr))),
        "Prepared session requires the exact local store for this PR and reviewer"
    );
    let path = store.directory.join(PREPARED_FILE);
    check_path(root, &path, false)?;
    let pointer: Active = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        pointer.version == VERSION,
        "Unsupported prepared session version"
    );
    ensure!(
        exact_path(&pointer.root, root)
            && pointer
                .directory
                .parent()
                .is_some_and(|parent| exact_path(parent, &store.directory)),
        "Prepared session belongs to another workspace or store"
    );
    check_digest(&pointer.manifest_sha256)?;
    let session = load_directory(root, &state_base, &pointer.directory)?;
    ensure!(
        session.manifest_sha256 == pointer.manifest_sha256,
        "Prepared session manifest changed"
    );
    session.require_live()?;
    ensure!(session.pr.same_revision(pr), "Prepared PR revision changed");
    Ok(session)
}

pub fn load_active(root: &Path, state_base: &Path) -> Result<Option<Session>> {
    let session = load_active_for_cleanup(root, state_base)?;
    if let Some(session) = &session {
        session.require_not_cancelled()?;
    }
    Ok(session)
}

// Cancellation must inspect a pointer left by an interrupted cancellation, but
// public callers must never navigate from that pointer to another review.
fn load_active_for_cleanup(root: &Path, state_base: &Path) -> Result<Option<Session>> {
    let root = workspace_root(root)?;
    check_state_base(&root, state_base)?;
    let path = state_base.join("active.json");
    if !exists_without_following(&path)? {
        return Ok(None);
    }
    check_path(&root, &path, false)?;
    let active: Active = serde_json::from_slice(&fs::read(&path)?)
        .context("Invalid active review session pointer")?;
    ensure!(
        active.version == VERSION,
        "Unsupported active session version"
    );
    ensure!(
        exact_path(&active.root, &root),
        "Active session belongs to another workspace root"
    );
    check_digest(&active.manifest_sha256)?;
    let session = load_directory(&root, state_base, &active.directory)?;
    ensure!(
        session.manifest_sha256 == active.manifest_sha256,
        "Active session manifest changed; prepare a new diff"
    );
    Ok(Some(session))
}

/// Archive and clear a local draft, then remove only validated head/base
/// snapshots for this PR and reviewer. The caller must hold workspace and Store
/// locks and obtain confirmation. No GitHub, Git, or jj operation is performed.
/// Returns the number of sessions marked or cleaned; completed retries return 0.
/// An absent draft is allowed for reviews without comments and interrupted
/// cancellation. Uncertain or submitted drafts must use recovery, not cancellation.
/// For an action from a selected tab, the caller must recheck load_for_cancel
/// under those locks; a Draft alone cannot distinguish old and current tabs.
pub fn cancel_review(root: &Path, store: &Store, draft: &Draft) -> Result<usize> {
    let root = workspace_root(root)?;
    let state_base = root.join(STATE_PATH);
    ensure!(
        exact_path(&store.directory, &state_base.join(draft_key(&draft.pr))),
        "Cancellation requires the exact local store for this PR and reviewer"
    );
    check_state_base(&root, &state_base)?;
    check_path(&root, &store.directory, true)?;
    check_cancellable(draft)?;
    let draft_path = store.directory.join("draft.json");
    if exists_without_following(&draft_path)? {
        check_path(&root, &draft_path, false)?;
    }
    let saved = store.load()?;
    if let Some(saved) = &saved {
        check_cancellable(saved)?;
        ensure!(
            serde_json::to_value(saved)? == serde_json::to_value(draft)?,
            "Persisted draft does not match the cancelled draft; all local state was kept"
        );
    }

    // Resolve every candidate and the pointer before publishing markers,
    // archiving, clearing the draft, or deleting even one snapshot.
    let sessions = load_store_sessions(&root, &state_base, &store.directory)?;
    let scope = session_hashes(&sessions);
    let active = load_active_for_cleanup(&root, &state_base)?;
    let remove_active = active.as_ref().is_some_and(|session| {
        session
            .directory
            .parent()
            .is_some_and(|parent| exact_path(parent, &store.directory))
    });
    let draft_sha256 = digest(&serde_json::to_vec(draft)?);
    let unmarked = sessions.iter().any(|session| session.cancelled.is_none());
    if (saved.is_some() || unmarked)
        && !sessions.iter().any(|session| {
            session
                .cancelled
                .as_ref()
                .is_some_and(|marker| marker.draft_sha256 == draft_sha256)
        })
    {
        store.archive(draft)?;
        // Store::archive syncs the file; also persist its directory entry before
        // publishing a marker that permits clearing the only working draft.
        #[cfg(unix)]
        File::open(&store.directory)?.sync_all()?;
    }
    // Publish all markers before clearing the draft. An interruption in this
    // loop still has the saved draft, and a retry can recognize its archive.
    let mut changed = BTreeSet::new();
    for session in &sessions {
        if session.cancelled.is_none() {
            write_cancelled(session, store, &draft_sha256, &scope)?;
            changed.insert(session.directory.clone());
        }
    }
    store.clear()?;
    for session in &sessions {
        if remove_snapshots(session)? {
            changed.insert(session.directory.clone());
        }
    }
    if remove_active {
        fs::remove_file(state_base.join("active.json"))?;
        #[cfg(unix)]
        File::open(&state_base)?.sync_all()?;
    }
    Ok(changed.len())
}

fn check_cancellable(draft: &Draft) -> Result<()> {
    ensure!(draft.version == 1, "Unsupported draft version");
    ensure!(
        draft.attempt.is_none() && draft.retry_history.is_empty(),
        "A submission may already have reached GitHub; run recover before cancelling this local review"
    );
    ensure!(
        draft.submitted.is_none(),
        "This draft was submitted; use recover and existing submitted cleanup to preserve its receipt, not cancel"
    );
    Ok(())
}

fn write_cancelled(
    session: &Session,
    store: &Store,
    draft_sha256: &str,
    sessions: &BTreeSet<String>,
) -> Result<()> {
    write_marker(
        session,
        store,
        CANCELLED_FILE,
        &Cancelled {
            version: VERSION,
            manifest_sha256: session.manifest_sha256.clone(),
            draft_sha256: draft_sha256.to_owned(),
            sessions: Some(sessions.clone()),
        },
    )
}

/// Remove only validated local snapshots after a successful, persisted review.
/// The caller must hold both the workspace lock and the Store lock. As with
/// Store writes, those locks do not protect against hostile filesystem writers.
/// Returns the number of sessions changed, including resumed partial cleanup;
/// a completed session is a no-op. No external store is inspected or modified.
pub fn cleanup_submitted(root: &Path, store: &Store, draft: &Draft) -> Result<usize> {
    let root = workspace_root(root)?;
    let state_base = root.join(STATE_PATH);
    if !exact_path(&store.directory, &state_base.join(draft_key(&draft.pr))) {
        return Ok(0);
    }
    check_state_base(&root, &state_base)?;
    check_path(&root, &store.directory, true)?;
    ensure!(
        draft.version == 1 && draft.attempt.is_none(),
        "Snapshot cleanup requires a successful review, not an uncertain submission"
    );
    let receipt = draft
        .submitted
        .as_ref()
        .context("Snapshot cleanup requires a submitted review receipt")?;
    check_receipt(receipt.id, &receipt.state)?;
    check_path(&root, &store.directory.join("draft.json"), false)?;
    let saved = store
        .load()?
        .context("No persisted review receipt for cleanup")?;
    ensure!(
        saved.attempt.is_none()
            && saved.pr.same_revision(&draft.pr)
            && saved.submitted.as_ref().is_some_and(|saved| {
                saved.id == receipt.id && saved.state == receipt.state && saved.url == receipt.url
            }),
        "Persisted review receipt does not match the submitted draft; snapshots were kept"
    );

    // Validate every candidate before changing any session. Other state files,
    // other PRs, and other reviewers are never traversed or removed.
    let mut sessions = Vec::new();
    for entry in fs::read_dir(&store.directory)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("diff-") {
            let session = load_directory(&root, &state_base, &entry.path())?;

            sessions.push(session);
        }
    }

    let mut cleaned = 0;
    for session in sessions {
        // The current persisted review authorizes cleanup for this PR/reviewer.
        // Keep earlier markers unchanged, even when resuming partial cleanup.
        let mut changed = session.cleanup.is_none() && session.cancelled.is_none();
        if changed {
            write_cleanup(&session, store, receipt)?;
        }
        changed |= remove_snapshots(&session)?;
        cleaned += usize::from(changed);
    }
    Ok(cleaned)
}

// All sessions and any active pointer must be validated before calling this.
// Missing files are authorized only by an existing cleanup/cancellation marker.
fn remove_snapshots(session: &Session) -> Result<bool> {
    let root = &session.root;
    let mut changed = false;
    let mut directories = BTreeSet::from([PathBuf::from("base"), PathBuf::from("head")]);
    for snapshot in session
        .manifest
        .files
        .iter()
        .flat_map(|file| file.base.iter().chain(file.head.iter()))
    {
        let path = session.directory.join(&snapshot.path);
        if exists_without_following(&path)? {
            check_path(root, &path, false)?;
            fs::remove_file(&path)
                .with_context(|| format!("Cannot remove snapshot {}", path.display()))?;
            #[cfg(unix)]
            File::open(path.parent().context("Snapshot has no parent")?)?.sync_all()?;
            changed = true;
        }
        let mut parent = Path::new(&snapshot.path).parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            directories.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    // Children sort after their parents. Remove only empty registered
    // directories, never recursively and never the diff directory itself.
    for relative in directories.iter().rev() {
        let path = session.directory.join(relative);
        if exists_without_following(&path)? {
            check_path(root, &path, true)?;
            fs::remove_dir(&path)
                .with_context(|| format!("Cannot remove snapshot directory {}", path.display()))?;
            #[cfg(unix)]
            File::open(path.parent().context("Snapshot directory has no parent")?)?.sync_all()?;
            changed = true;
        }
    }
    Ok(changed)
}

fn check_receipt(id: u64, state: &str) -> Result<()> {
    // Recovery verifies the original accepted review even if GitHub has since
    // dismissed it. Cleanup still requires the matching receipt on disk.
    ensure!(
        id > 0
            && matches!(
                state,
                "COMMENTED" | "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
            ),
        "Snapshot cleanup requires a valid comment, approval, request-changes, or recovered dismissed receipt"
    );
    Ok(())
}

fn write_cleanup(session: &Session, store: &Store, receipt: &Receipt) -> Result<()> {
    let cleanup = Cleanup {
        version: VERSION,
        manifest_sha256: session.manifest_sha256.clone(),
        review_id: receipt.id,
        review_state: receipt.state.clone(),
        review_url: receipt.url.clone(),
    };
    write_marker(session, store, CLEANUP_FILE, &cleanup)
}

fn write_marker(
    session: &Session,
    store: &Store,
    name: &str,
    marker: &impl Serialize,
) -> Result<()> {
    let path = session.directory.join(name);
    ensure!(
        !exists_without_following(&path)?,
        "Snapshot cleanup/cancellation marker already exists; reload the session before cleanup"
    );
    // A crash before publication must not leave an unregistered temporary file
    // inside the immutable session and prevent a later cleanup attempt.
    let mut temporary = tempfile::NamedTempFile::new_in(&store.directory)?;
    serde_json::to_writer_pretty(&mut temporary, marker)?;
    temporary.write_all(b"\n")?;
    let mut permissions = temporary.as_file().metadata()?.permissions();
    permissions.set_readonly(true);
    temporary.as_file().set_permissions(permissions)?;
    temporary.as_file().sync_all()?;
    // The caller holds both locks and checked that no marker exists. Use an
    // atomic rename, not link/unlink publication: a crash between those steps
    // could leave a hard-linked marker that cannot safely be loaded again.
    temporary.persist(&path).map_err(|error| error.error)?;
    #[cfg(unix)]
    {
        File::open(&session.directory)?.sync_all()?;
        File::open(&store.directory)?.sync_all()?;
    }
    Ok(())
}

fn create_session(
    root: &Path,
    store: &Store,
    pr: &PullRequest,
    merge_base: &str,
    files: &[ChangedFile],
    mut blob: impl FnMut(&str, &str) -> Result<Vec<u8>>,
) -> Result<Session> {
    let state_base = root.join(STATE_PATH);
    check_state_base(root, &state_base)?;
    ensure!(
        exact_path(&store.directory, &state_base.join(draft_key(pr))),
        "Snapshot store must be the workspace review directory for this PR and reviewer"
    );
    check_path(root, &store.directory, true)?;
    check_sha(&pr.head_sha)?;
    check_sha(&pr.base_sha)?;
    check_sha(merge_base)?;
    // Cleanup enumerates only diff-* directories. A killed builder must leave
    // staging data, never a partially written session that blocks later cleanup.
    let temporary = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(&store.directory)?;
    let staging = temporary.path();
    let suffix = staging
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(STAGING_PREFIX))
        .context("Invalid staging directory name")?;
    let directory = store.directory.join(format!("diff-{suffix}"));
    fs::create_dir(staging.join("base"))?;
    fs::create_dir(staging.join("head"))?;
    let mut entries = Vec::new();
    let mut filenames = BTreeSet::new();
    for file in files {
        let (base_path, head_path) = sides(
            &file.filename,
            &file.status,
            file.previous_filename.as_deref(),
        )?;
        ensure!(
            filenames.insert(&file.filename),
            "Duplicate GitHub filename"
        );
        let mut binary = false;
        let mut write = |side: &str, sha: &str, path: &str| -> Result<Snapshot> {
            let bytes = blob(sha, path)
                .with_context(|| format!("Cannot prepare {side} snapshot for {path}"))?;
            binary |= bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
            let relative = format!("{side}/{path}");
            write_snapshot(root, staging, &relative, &bytes)?;
            Ok(Snapshot {
                path: relative,
                repository_path: path.to_owned(),
                sha256: digest(&bytes),
            })
        };
        let base = base_path
            .map(|path| write("base", merge_base, path))
            .transpose()?;
        let head = head_path
            .map(|path| write("head", &pr.head_sha, path))
            .transpose()?;
        entries.push(ManifestFile {
            filename: file.filename.clone(),
            status: file.status.clone(),
            previous_filename: file.previous_filename.clone(),
            has_patch: file
                .patch
                .as_ref()
                .is_some_and(|patch| !patch.trim().is_empty()),
            patch: file.patch.clone(),
            binary,
            base,
            head,
        });
    }
    let manifest = Manifest {
        version: VERSION,
        root: root.to_path_buf(),
        directory: directory.clone(),
        pr: pr.clone(),
        merge_base: merge_base.to_owned(),
        patches_cached: true,
        files: entries,
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    write_snapshot(root, staging, "manifest.json", &bytes)?;
    let session = load_directory_from(root, &state_base, &directory, staging)?;
    session.require_live()?;
    // The Store lock serializes publication. Never replace an existing session,
    // even in the unlikely event that a random suffix is reused.
    ensure!(
        !exists_without_following(&directory)?,
        "Session destination already exists; no snapshots were replaced"
    );
    fs::rename(staging, &directory).context("Cannot publish prepared snapshots")?;
    let published = (|| -> Result<()> {
        check_path(root, &directory, true)?;
        #[cfg(unix)]
        File::open(&store.directory)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = published {
        // Move back before TempDir removes files, so interruption during error
        // cleanup cannot expose a partially removed diff-* directory either.
        fs::rename(&directory, staging).with_context(|| {
            format!("Cannot withdraw session after publication failed: {error:#}")
        })?;
        return Err(error);
    }
    // A later PR recheck can fail, but the published session is complete and
    // inactive. Existing tabs and pointers remain unchanged.
    let _ = temporary.keep();
    Ok(session)
}

fn sides<'a>(
    filename: &'a str,
    status: &str,
    previous: Option<&'a str>,
) -> Result<(Option<&'a str>, Option<&'a str>)> {
    safe_relative(filename)?;
    if let Some(previous) = previous {
        safe_relative(previous)?;
        ensure!(
            previous != filename,
            "Rename/copy source equals its destination"
        );
    }
    if matches!(status, "renamed" | "copied") {
        ensure!(
            previous.is_some(),
            "Rename/copy is missing its original filename"
        );
    } else {
        ensure!(
            previous.is_none(),
            "Unexpected original filename for {status}"
        );
    }
    match status {
        "added" | "copied" => Ok((None, Some(filename))),
        "removed" => Ok((Some(filename), None)),
        "renamed" => Ok((previous, Some(filename))),
        "modified" | "changed" | "unchanged" => Ok((Some(filename), Some(filename))),
        _ => bail!("Unsupported GitHub file status: {status:?}"),
    }
}

fn load_directory(root: &Path, state_base: &Path, directory: &Path) -> Result<Session> {
    load_directory_from(root, state_base, directory, directory)
}

// Only creation supplies separate staging contents. The manifest and returned
// identity always name the final diff-* directory; ordinary loaders cannot use
// a staging directory as a session or relax any snapshot/path validation.
fn load_directory_from(
    root: &Path,
    state_base: &Path,
    directory: &Path,
    contents: &Path,
) -> Result<Session> {
    check_state_base(root, state_base)?;
    let relative = directory
        .strip_prefix(state_base)
        .context("Session directory is outside workspace review state")?;
    let relative = relative
        .to_str()
        .context("Session directory must be UTF-8")?;
    safe_relative(relative)?;
    let components: Vec<_> = relative.split('/').collect();
    ensure!(
        components.len() == 2
            && components[1].starts_with("diff-")
            && components[1].len() > 5
            && components[1]
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid session directory layout; expected <draft-key>/diff-<unique>"
    );
    if !exact_path(directory, contents) {
        let suffix = components[1].strip_prefix("diff-").unwrap();
        ensure!(
            exact_path(
                contents,
                &state_base
                    .join(components[0])
                    .join(format!("{STAGING_PREFIX}{suffix}"))
            ),
            "Staging directory does not match the final session path"
        );
    }
    check_path(root, contents, true)?;
    let path = contents.join("manifest.json");
    check_path(root, &path, false)?;
    let bytes = fs::read(&path).context("Cannot read session manifest")?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("Invalid session manifest")?;
    ensure!(
        manifest.version == VERSION,
        "Unsupported session manifest version"
    );
    ensure!(
        exact_path(&manifest.root, root) && exact_path(&manifest.directory, directory),
        "Manifest workspace root or immutable session directory does not match"
    );
    ensure!(
        components[0] == draft_key(&manifest.pr),
        "Session directory does not match the PR identity and reviewer"
    );
    ensure!(
        manifest.pr.number > 0,
        "Invalid PR number in session manifest"
    );
    check_sha(&manifest.pr.head_sha)?;
    check_sha(&manifest.pr.base_sha)?;
    check_sha(&manifest.merge_base)?;
    let manifest_sha256 = digest(&bytes);
    let cleanup_path = contents.join(CLEANUP_FILE);
    let cleanup = if exists_without_following(&cleanup_path)? {
        check_path(root, &cleanup_path, false)?;
        let cleanup: Cleanup = serde_json::from_slice(&fs::read(&cleanup_path)?)
            .context("Invalid snapshot cleanup marker")?;
        ensure!(
            cleanup.version == VERSION,
            "Unsupported cleanup marker version"
        );
        check_digest(&cleanup.manifest_sha256)?;
        ensure!(
            cleanup.manifest_sha256 == manifest_sha256,
            "Cleanup marker does not match the immutable session manifest"
        );
        check_receipt(cleanup.review_id, &cleanup.review_state)?;
        Some(cleanup)
    } else {
        None
    };
    let cancelled_path = contents.join(CANCELLED_FILE);
    let cancelled = if exists_without_following(&cancelled_path)? {
        check_path(root, &cancelled_path, false)?;
        let cancelled: Cancelled = serde_json::from_slice(&fs::read(&cancelled_path)?)
            .context("Invalid local review cancellation marker")?;
        ensure!(
            cancelled.version == VERSION,
            "Unsupported cancellation marker version"
        );
        check_digest(&cancelled.manifest_sha256)?;
        check_digest(&cancelled.draft_sha256)?;
        if let Some(sessions) = &cancelled.sessions {
            ensure!(
                sessions.contains(&manifest_sha256),
                "Cancellation scope does not contain its immutable session manifest"
            );
            for sha256 in sessions {
                check_digest(sha256)?;
            }
        }
        ensure!(
            cancelled.manifest_sha256 == manifest_sha256,
            "Cancellation marker does not match the immutable session manifest"
        );
        Some(cancelled)
    } else {
        None
    };
    let removed = cleanup.is_some() || cancelled.is_some();
    let mut actual_files = BTreeSet::new();
    let mut actual_dirs = BTreeSet::new();
    inventory(
        root,
        contents,
        contents,
        &mut actual_files,
        &mut actual_dirs,
    )?;
    let mut expected_files = BTreeSet::from(["manifest.json".to_owned()]);
    if cleanup.is_some() {
        expected_files.insert(CLEANUP_FILE.to_owned());
    }
    if cancelled.is_some() {
        expected_files.insert(CANCELLED_FILE.to_owned());
    }
    let mut expected_dirs = BTreeSet::from(["base".to_owned(), "head".to_owned()]);
    let mut filenames = BTreeSet::new();
    for file in &manifest.files {
        ensure!(
            filenames.insert(&file.filename),
            "Duplicate manifest filename"
        );
        ensure!(
            if manifest.patches_cached {
                file.has_patch
                    == file
                        .patch
                        .as_ref()
                        .is_some_and(|patch| !patch.trim().is_empty())
            } else {
                file.patch.is_none()
            },
            "Inconsistent cached GitHub patch metadata"
        );
        let (base, head) = sides(
            &file.filename,
            &file.status,
            file.previous_filename.as_deref(),
        )?;
        let mut binary = false;
        let mut complete = true;
        for (side, expected, snapshot) in [
            ("base", base, file.base.as_ref()),
            ("head", head, file.head.as_ref()),
        ] {
            ensure!(
                expected.is_some() == snapshot.is_some(),
                "Manifest {side} membership does not match file status"
            );
            if let (Some(repository_path), Some(snapshot)) = (expected, snapshot) {
                ensure!(
                    snapshot.repository_path == repository_path
                        && snapshot.path == format!("{side}/{repository_path}"),
                    "Snapshot path must mirror its exact repository filename"
                );
                check_digest(&snapshot.sha256)?;
                ensure!(
                    expected_files.insert(snapshot.path.clone()),
                    "Duplicate snapshot path in manifest"
                );
                let mut parent = Path::new(&snapshot.path).parent();
                while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
                    expected_dirs.insert(
                        path.to_str()
                            .context("Invalid snapshot directory")?
                            .to_owned(),
                    );
                    parent = path.parent();
                }
                if removed && !actual_files.contains(&snapshot.path) {
                    complete = false;
                    continue;
                }
                let path = contents.join(&snapshot.path);
                check_path(root, &path, false)?;
                let content = fs::read(&path)?;
                ensure!(
                    digest(&content) == snapshot.sha256,
                    "Snapshot content changed: {}; prepare a new diff",
                    path.display()
                );
                binary |= content.contains(&0) || std::str::from_utf8(&content).is_err();
            }
        }
        ensure!(
            (!complete || file.binary == binary) && (!binary || file.binary),
            "Incorrect binary metadata in session manifest"
        );
    }
    ensure!(
        expected_files.is_disjoint(&expected_dirs),
        "Manifest registers a path as both a file and a directory"
    );
    let valid_inventory = if removed {
        actual_files.is_subset(&expected_files) && actual_dirs.is_subset(&expected_dirs)
    } else {
        expected_files == actual_files && expected_dirs == actual_dirs
    };
    ensure!(
        valid_inventory,
        "Session contains missing, unregistered, or aliased snapshot files/directories"
    );
    Ok(Session {
        pr: manifest.pr.clone(),
        root: root.to_path_buf(),
        directory: directory.to_path_buf(),
        merge_base: manifest.merge_base.clone(),
        manifest,
        manifest_sha256,
        cleanup,
        cancelled,
    })
}

fn inventory(
    root: &Path,
    session: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
    directories: &mut BTreeSet<String>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(session)?
            .to_str()
            .context("Snapshot path must be UTF-8")?
            .to_owned();
        safe_relative(&relative)?;
        let is_dir = entry.file_type()?.is_dir();
        check_path(root, &path, is_dir)?;
        if is_dir {
            directories.insert(relative);
            inventory(root, session, &path, files, directories)?;
        } else {
            files.insert(relative);
        }
    }
    Ok(())
}

fn workspace_root(root: &Path) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .context("Cannot resolve workspace root")?;
    ensure!(root.is_dir(), "Workspace root is not a directory");
    Ok(root)
}

fn exact_path(left: &Path, right: &Path) -> bool {
    // Path equality normalizes some aliases (repeated slashes and dot segments).
    left.as_os_str() == right.as_os_str()
}

fn safe_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && !path.split('/').any(|part| matches!(part, "" | "." | ".."))
            && !(path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic()),
        "Unsafe repository/snapshot path: {path:?}"
    );
    Ok(())
}

fn exists_without_following(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Cannot inspect {}", path.display())),
    }
}

fn check_state_base(root: &Path, state_base: &Path) -> Result<()> {
    ensure!(
        exact_path(state_base, &root.join(STATE_PATH)),
        "Review snapshots must be stored at ROOT/scratch/zed-pr-review"
    );
    // A workspace without any state is valid. Still reject dangling symlinks
    // and aliases in every existing ancestor instead of treating them as absent.
    for directory in [root.join("scratch"), state_base.to_path_buf()] {
        if !exists_without_following(&directory)? {
            break;
        }
        check_path(root, &directory, true)?;
    }
    Ok(())
}

/// Require canonical spelling and inspect each component below the canonical
/// workspace root. Checking only canonicalize() would silently follow symlinks.
fn check_path(root: &Path, path: &Path, directory: bool) -> Result<()> {
    ensure!(path.is_absolute(), "Snapshot/state path must be absolute");
    ensure!(
        exact_path(&root.canonicalize()?, root),
        "Workspace root is no longer canonical"
    );
    let relative = path
        .strip_prefix(root)
        .context("Path is outside the workspace root")?;
    let relative = relative
        .to_str()
        .context("Snapshot/state path must be UTF-8")?;
    safe_relative(relative)?;
    ensure!(
        exact_path(path, &root.join(relative)),
        "Snapshot/state path uses a noncanonical spelling"
    );
    let mut checked = root.to_path_buf();
    let components: Vec<_> = relative.split('/').collect();
    for (index, component) in components.iter().enumerate() {
        // This also rejects case-folding and Unicode-normalization aliases on
        // filesystems where canonicalize() can preserve the requested spelling.
        let exact_name = fs::read_dir(&checked)?.try_fold(false, |found, entry| {
            Ok::<_, std::io::Error>(found || entry?.file_name() == *component)
        })?;
        ensure!(
            exact_name,
            "Path component is missing or aliased: {component:?}"
        );
        checked.push(component);
        let metadata = fs::symlink_metadata(&checked)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Symlinks are not allowed in review state: {}",
            checked.display()
        );
        if index + 1 < components.len() || directory {
            ensure!(metadata.is_dir(), "Expected snapshot/state directory");
        } else {
            ensure!(metadata.is_file(), "Expected a regular snapshot/state file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.nlink() == 1,
                    "Hard-linked snapshot/state files are not allowed"
                );
            }
        }
    }
    ensure!(
        exact_path(&path.canonicalize()?, path),
        "Snapshot/state path is an alias"
    );
    Ok(())
}

fn write_snapshot(root: &Path, directory: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    safe_relative(relative)?;
    let path = directory.join(relative);
    let parent = path.parent().context("Snapshot has no parent directory")?;
    let mut checked = directory.to_path_buf();
    check_path(root, &checked, true)?;
    for component in parent.strip_prefix(directory)?.components() {
        checked.push(component);
        if !exists_without_following(&checked)? {
            fs::create_dir(&checked)?;
        }
        check_path(root, &checked, true)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("Cannot create unique snapshot {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn check_sha(sha: &str) -> Result<()> {
    ensure!(
        matches!(sha.len(), 40 | 64) && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "Session requires full hexadecimal Git object IDs"
    );
    Ok(())
}

fn check_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "Invalid SHA256 digest in session state"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        state: PathBuf,
        store: Store,
    }

    fn pr() -> PullRequest {
        PullRequest {
            host: "github.com".into(),
            repo: "owner/repo".into(),
            number: 42,
            url: "https://github.com/owner/repo/pull/42".into(),
            title: "Review".into(),
            head_sha: "a".repeat(40),
            base_sha: "b".repeat(40),
            head_repo: "owner/repo".into(),
            head_ref: "topic".into(),
            author: "author".into(),
            reviewer: "reviewer".into(),
        }
    }

    fn changed(filename: &str, status: &str, previous: Option<&str>) -> ChangedFile {
        ChangedFile {
            filename: filename.into(),
            status: status.into(),
            previous_filename: previous.map(str::to_owned),
            patch: Some("@@ -1 +1 @@\n-old\n+new".into()),
        }
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let state = root.join(STATE_PATH);
            let store = Store::open(&state, &pr()).unwrap();
            Self {
                _temporary: temporary,
                root,
                state,
                store,
            }
        }

        fn session(&self) -> Session {
            self.with_files(&pr(), &[changed("src/example.rs", "modified", None)])
        }

        fn submitted(&self, state: &str) -> Draft {
            let mut draft = Draft::new(pr());
            draft.submitted = Some(Receipt {
                id: 123,
                url: format!("{}#pullrequestreview-123", draft.pr.url),
                state: state.into(),
            });
            self.store.save(&draft).unwrap();
            draft
        }

        fn with_files(&self, pr: &PullRequest, files: &[ChangedFile]) -> Session {
            create_session(
                &self.root,
                &self.store,
                pr,
                &"c".repeat(40),
                files,
                |sha, _| {
                    Ok(if sha == pr.head_sha {
                        b"new\n"
                    } else {
                        b"old\n"
                    }
                    .to_vec())
                },
            )
            .unwrap()
        }
    }

    fn replace(path: &Path, bytes: &[u8]) {
        fs::remove_file(path).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn change_manifest(session: &Session, change: impl FnOnce(&mut Manifest)) {
        let mut manifest = session.manifest.clone();
        change(&mut manifest);
        replace(
            &session.directory.join("manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        );
    }

    fn archives(store: &Store) -> Vec<PathBuf> {
        fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("archive-")
            })
            .collect()
    }

    fn assert_cancelled<T: std::fmt::Debug>(result: Result<T>) {
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("cancelled") && error.contains("reopen"),
            "{error}"
        );
    }

    fn mark_cancelled(f: &Fixture, session: &Session, draft: &Draft) {
        f.store.archive(draft).unwrap();
        write_cancelled(
            session,
            &f.store,
            &digest(&serde_json::to_vec(draft).unwrap()),
            &session_hashes(&load_store_sessions(&f.root, &f.state, &f.store.directory).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn cancellation_archives_comments_or_empty_drafts_and_keeps_unrelated_metadata() {
        for comments in [false, true] {
            for saved in [false, true] {
                let f = Fixture::new();
                let session = f.session();
                session.activate(&f.state).unwrap();
                let mut draft = Draft::new(pr());
                if comments {
                    draft.comments.push(crate::model::Comment {
                        path: "src/example.rs".into(),
                        start_line: 1,
                        line: 1,
                        column: 1,
                        selected_text: "new".into(),
                        body: "Keep this archived comment".into(),
                    });
                }
                if saved {
                    f.store.save(&draft).unwrap();
                }
                let retained = [
                    f.store.directory.join("receipt.json"),
                    f.store.directory.join("comments.md"),
                    f.store.directory.join("message.txt"),
                    f.state.join("last-checkout.json"),
                    f.state.join("bookmarks.json"),
                ];
                for path in &retained {
                    fs::write(path, b"untouched local metadata").unwrap();
                }
                let manifest = fs::read(session.directory.join("manifest.json")).unwrap();
                assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 1);
                assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 0);
                assert!(f.store.load().unwrap().is_none());
                assert!(!f.state.join("active.json").exists());
                assert!(!session.head_root().exists());
                assert!(!session.directory.join("base").exists());
                assert!(!session.directory.join(CLEANUP_FILE).exists());
                assert_eq!(fs::read_dir(&session.directory).unwrap().count(), 2);
                assert_eq!(
                    fs::read(session.directory.join("manifest.json")).unwrap(),
                    manifest
                );
                let archived = archives(&f.store);
                assert_eq!(archived.len(), 1);
                let archived: Draft =
                    serde_json::from_slice(&fs::read(&archived[0]).unwrap()).unwrap();
                assert_eq!(
                    serde_json::to_value(archived).unwrap(),
                    serde_json::to_value(&draft).unwrap()
                );
                for path in retained {
                    assert_eq!(fs::read(path).unwrap(), b"untouched local metadata");
                }
            }
        }
    }

    #[test]
    fn cancellation_covers_revisions_without_an_active_pointer_and_preserves_other_stores() {
        for active_other in [false, true] {
            let f = Fixture::new();
            let first = f.session();
            let mut newer = pr();
            newer.head_sha = "d".repeat(40);
            let second = f.with_files(&newer, &[changed("src/example.rs", "modified", None)]);
            let mut other_sessions = Vec::new();
            let mut other_drafts = Vec::new();
            for reviewer in [false, true] {
                let mut other = pr();
                if reviewer {
                    other.reviewer = "other-reviewer".into();
                } else {
                    other.number += 1;
                }
                let store = Store::open(&f.state, &other).unwrap();
                store.save(&Draft::new(other.clone())).unwrap();
                let path = store.directory.join("draft.json");
                other_drafts.push((path.clone(), fs::read(path).unwrap()));
                other_sessions.push(
                    create_session(
                        &f.root,
                        &store,
                        &other,
                        &"c".repeat(40),
                        &[changed("src/example.rs", "modified", None)],
                        |_, _| Ok(b"other\n".to_vec()),
                    )
                    .unwrap(),
                );
            }
            if active_other {
                other_sessions[0].activate(&f.state).unwrap();
            }
            let active = fs::read(f.state.join("active.json")).ok();
            let draft = Draft::new(newer);
            f.store.save(&draft).unwrap();
            assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 2);
            for session in [&first, &second] {
                assert!(!session.head_root().exists());
                assert_cancelled(session.validate());
            }
            assert_eq!(fs::read(f.state.join("active.json")).ok(), active);
            for session in other_sessions {
                session.validate().unwrap();
            }
            for (path, bytes) in other_drafts {
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn cancellation_retries_partial_marker_publication_and_draft_clear() {
        for clear_draft in [false, true] {
            let f = Fixture::new();
            let first = f.session();
            let second = f.session();
            first.activate(&f.state).unwrap();
            let draft = Draft::new(pr());
            f.store.save(&draft).unwrap();
            mark_cancelled(&f, &first, &draft);
            let marker = fs::read(first.directory.join(CANCELLED_FILE)).unwrap();
            if clear_draft {
                write_cancelled(
                    &second,
                    &f.store,
                    &digest(&serde_json::to_vec(&draft).unwrap()),
                    &session_hashes(
                        &load_store_sessions(&f.root, &f.state, &f.store.directory).unwrap(),
                    ),
                )
                .unwrap();
                f.store.clear().unwrap();
            }
            assert_cancelled(load_active(&f.root, &f.state));
            for session in [&first, &second] {
                for side in ["head", "base"] {
                    let path = session.directory.join(side).join("src/example.rs");
                    assert_eq!(
                        load_for_cancel(&f.root, &f.state, &path)
                            .unwrap()
                            .unwrap()
                            .directory,
                        session.directory
                    );
                }
            }
            assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 2);
            assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 0);
            assert_eq!(archives(&f.store).len(), 1);
            assert_eq!(
                fs::read(first.directory.join(CANCELLED_FILE)).unwrap(),
                marker
            );
            assert!(f.store.load().unwrap().is_none());
            assert!(!f.state.join("active.json").exists());
        }
    }

    #[test]
    fn cancellation_resumes_at_every_snapshot_deletion_boundary() {
        for removed in 0..=6 {
            let f = Fixture::new();
            let session = f.session();
            session.activate(&f.state).unwrap();
            let mut draft = Draft::new(pr());
            draft.comments.push(crate::model::Comment {
                path: "src/example.rs".into(),
                start_line: 1,
                line: 1,
                column: 1,
                selected_text: "new".into(),
                body: "archived".into(),
            });
            f.store.save(&draft).unwrap();
            mark_cancelled(&f, &session, &draft);
            f.store.clear().unwrap();
            let marker = fs::read(session.directory.join(CANCELLED_FILE)).unwrap();
            let paths = [
                "base/src/example.rs",
                "head/src/example.rs",
                "base/src",
                "head/src",
                "base",
                "head",
            ];
            for (index, relative) in paths[..removed].iter().enumerate() {
                let path = session.directory.join(relative);
                if index < 2 {
                    fs::remove_file(path).unwrap();
                } else {
                    fs::remove_dir(path).unwrap();
                }
            }
            for side in ["head", "base"] {
                let path = session.directory.join(side).join("src/example.rs");
                let loaded = load_for_cancel(&f.root, &f.state, &path).unwrap().unwrap();
                assert_eq!(loaded.directory, session.directory);
                assert_eq!(loaded.pr, session.pr);
                assert_cancelled(loaded.validate());
                assert_cancelled(load_for_navigation(&f.root, &f.state, &path));
            }
            // A CLI retry reconstructs an empty draft once draft.json is gone.
            let retry = Draft::new(pr());
            assert_eq!(
                cancel_review(&f.root, &f.store, &retry).unwrap(),
                usize::from(removed < 6)
            );
            assert_eq!(cancel_review(&f.root, &f.store, &retry).unwrap(), 0);
            assert_eq!(archives(&f.store).len(), 1);
            assert_eq!(
                fs::read(session.directory.join(CANCELLED_FILE)).unwrap(),
                marker
            );
            assert!(!f.state.join("active.json").exists());
        }
    }

    #[test]
    fn cancelled_tabs_never_rebind_but_new_reviews_can_open_and_submit() {
        let f = Fixture::new();
        let old = f.session();
        old.activate(&f.state).unwrap();
        cancel_review(&f.root, &f.store, &Draft::new(pr())).unwrap();
        let marker = fs::read(old.directory.join(CANCELLED_FILE)).unwrap();
        let new = f.session();
        new.activate(&f.state).unwrap();
        new.validate().unwrap();
        assert_eq!(
            new.selection_path(&new.head_root().join("src/example.rs"))
                .unwrap(),
            "src/example.rs"
        );
        assert_eq!(
            load_active(&f.root, &f.state).unwrap().unwrap().directory,
            new.directory
        );
        assert_cancelled(old.validate());
        assert_cancelled(old.open());
        assert_cancelled(old.activate(&f.state));
        assert_cancelled(old.selection_path(&old.head_root().join("src/example.rs")));
        assert_cancelled(load_for_file(
            &f.root,
            &f.state,
            &old.head_root().join("src/example.rs"),
        ));
        for side in ["head", "base"] {
            let path = old.directory.join(side).join("src/example.rs");
            assert_cancelled(load_for_navigation(&f.root, &f.state, &path));
            assert!(load_for_cancel(&f.root, &f.state, &path).is_err());
        }
        let draft = f.submitted("COMMENTED");
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 1);
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 0);
        assert_eq!(
            fs::read(old.directory.join(CANCELLED_FILE)).unwrap(),
            marker
        );
        assert!(!old.directory.join(CLEANUP_FILE).exists());
        let cleanup: Cleanup =
            serde_json::from_slice(&fs::read(new.directory.join(CLEANUP_FILE)).unwrap()).unwrap();
        assert_eq!(cleanup.review_state, "COMMENTED");
    }

    // Mirror the CLI: load exact selected context, then load the current draft.
    fn cancel_from_file(f: &Fixture, path: &Path) -> Result<usize> {
        let session = load_for_cancel(&f.root, &f.state, path)?.context("Not a review session")?;
        let draft = f.store.load()?.unwrap_or_else(|| Draft::new(session.pr));
        cancel_review(&f.root, &f.store, &draft)
    }

    #[test]
    fn old_cancelled_tabs_cannot_abandon_a_new_review_of_the_same_revision() {
        for saved in [false, true] {
            for activated in [false, true] {
                let f = Fixture::new();
                let old = f.session();
                cancel_from_file(&f, &old.head_root().join("src/example.rs")).unwrap();
                let new = f.session();
                assert!(old.pr.same_revision(&new.pr));
                if activated {
                    new.activate(&f.state).unwrap();
                }
                if saved {
                    f.store.save(&Draft::new(pr())).unwrap();
                }
                // Even an absent or byte-identical empty draft must not let an
                // old tab reach the newly opened session.
                let draft = fs::read(f.store.directory.join("draft.json")).ok();
                let active = fs::read(f.state.join("active.json")).ok();
                let archive_count = archives(&f.store).len();
                let marker = fs::read(old.directory.join(CANCELLED_FILE)).unwrap();
                for side in ["head", "base"] {
                    let error =
                        cancel_from_file(&f, &old.directory.join(side).join("src/example.rs"))
                            .unwrap_err()
                            .to_string();
                    assert!(error.contains("earlier cancelled review"), "{error}");
                }
                new.validate().unwrap();
                assert!(!new.directory.join(CANCELLED_FILE).exists());
                assert_eq!(archives(&f.store).len(), archive_count);
                assert_eq!(fs::read(f.store.directory.join("draft.json")).ok(), draft);
                assert_eq!(fs::read(f.state.join("active.json")).ok(), active);
                assert_eq!(
                    fs::read(old.directory.join(CANCELLED_FILE)).unwrap(),
                    marker
                );
                // Explicit cancellation from the new review still works, and
                // its own tab can retry without changing the older marker.
                let path = new.head_root().join("src/example.rs");
                assert_eq!(cancel_from_file(&f, &path).unwrap(), 1);
                assert_eq!(cancel_from_file(&f, &path).unwrap(), 0);
                assert_eq!(
                    fs::read(old.directory.join(CANCELLED_FILE)).unwrap(),
                    marker
                );
            }
        }
    }

    #[test]
    fn partial_cancellation_cannot_expand_to_fresh_sessions_from_marked_or_unmarked_tabs() {
        for removed in [false, true] {
            let f = Fixture::new();
            let first = f.session();
            let second = f.session();
            let draft = Draft::new(pr());
            f.store.save(&draft).unwrap();
            mark_cancelled(&f, &first, &draft);
            if removed {
                fs::remove_file(first.head_root().join("src/example.rs")).unwrap();
            }
            // Both original tabs can retry while only the original sessions exist.
            for session in [&first, &second] {
                assert!(
                    load_for_cancel(
                        &f.root,
                        &f.state,
                        &session.head_root().join("src/example.rs")
                    )
                    .unwrap()
                    .is_some()
                );
            }
            let fresh = f.session();
            fresh.activate(&f.state).unwrap();
            let active = fs::read(f.state.join("active.json")).unwrap();
            let saved = fs::read(f.store.directory.join("draft.json")).unwrap();
            for session in [&first, &second] {
                assert!(cancel_from_file(&f, &session.head_root().join("src/example.rs")).is_err());
            }
            second.validate().unwrap();
            fresh.validate().unwrap();
            assert_eq!(fs::read(f.state.join("active.json")).unwrap(), active);
            assert_eq!(
                fs::read(f.store.directory.join("draft.json")).unwrap(),
                saved
            );
            assert_eq!(archives(&f.store).len(), 1);
            assert!(!second.directory.join(CANCELLED_FILE).exists());
            assert!(!fresh.directory.join(CANCELLED_FILE).exists());
        }
    }

    #[test]
    fn cancelled_context_cannot_clear_a_replacement_draft_without_new_snapshots() {
        let f = Fixture::new();
        let session = f.session();
        let path = session.head_root().join("src/example.rs");
        cancel_from_file(&f, &path).unwrap();
        let mut draft = Draft::new(pr());
        draft.comments.push(crate::model::Comment {
            path: "src/example.rs".into(),
            start_line: 1,
            line: 1,
            column: 1,
            selected_text: "new".into(),
            body: "Keep the replacement draft".into(),
        });
        f.store.save(&draft).unwrap();
        let saved = fs::read(f.store.directory.join("draft.json")).unwrap();
        let error = cancel_from_file(&f, &path).unwrap_err().to_string();
        assert!(error.contains("draft changed"), "{error}");
        assert_eq!(
            fs::read(f.store.directory.join("draft.json")).unwrap(),
            saved
        );
        assert_eq!(archives(&f.store).len(), 1);
    }

    #[test]
    fn legacy_cancelled_markers_cannot_authorize_unmarked_sessions() {
        let f = Fixture::new();
        let old = f.session();
        let path = old.head_root().join("src/example.rs");
        cancel_from_file(&f, &path).unwrap();
        let marker_path = old.directory.join(CANCELLED_FILE);
        let mut marker: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.as_object_mut().unwrap().remove("sessions");
        replace(&marker_path, &serde_json::to_vec(&marker).unwrap());
        assert_eq!(cancel_from_file(&f, &path).unwrap(), 0);
        let new = f.session();
        assert!(cancel_from_file(&f, &path).is_err());
        new.validate().unwrap();
        assert!(
            load_for_cancel(&f.root, &f.state, &new.head_root().join("src/example.rs"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn cancellation_scope_requires_valid_manifest_hashes_and_its_own_session() {
        for scope in [
            serde_json::json!([]),
            serde_json::json!(["invalid"]),
            serde_json::json!(["0".repeat(64)]),
        ] {
            let f = Fixture::new();
            let session = f.session();
            mark_cancelled(&f, &session, &Draft::new(pr()));
            let path = session.directory.join(CANCELLED_FILE);
            let mut marker: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            marker["sessions"] = scope;
            replace(&path, &serde_json::to_vec(&marker).unwrap());
            assert!(cancel_from_file(&f, &session.head_root().join("src/example.rs")).is_err());
            assert!(cancel_review(&f.root, &f.store, &Draft::new(pr())).is_err());
            assert!(session.head_root().join("src/example.rs").exists());
        }
    }

    #[test]
    fn submitted_cleanup_resumes_partial_cancellation_without_creating_a_receipt_for_it() {
        let f = Fixture::new();
        let old = f.session();
        mark_cancelled(&f, &old, &Draft::new(pr()));
        fs::remove_file(old.head_root().join("src/example.rs")).unwrap();
        let marker = fs::read(old.directory.join(CANCELLED_FILE)).unwrap();
        let new = f.session();
        let draft = f.submitted("COMMENTED");
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 2);
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 0);
        assert!(!new.head_root().exists());
        assert!(!old.head_root().exists());
        assert!(!old.directory.join(CLEANUP_FILE).exists());
        assert_eq!(
            fs::read(old.directory.join(CANCELLED_FILE)).unwrap(),
            marker
        );
    }

    #[test]
    fn cancellation_rejects_uncertain_submitted_or_mismatched_drafts_before_any_write() {
        for case in [
            "attempt",
            "history",
            "submitted",
            "version",
            "disk attempt",
            "disk history",
            "disk submitted",
            "disk comments",
            "disk revision",
            "disk identity",
        ] {
            let f = Fixture::new();
            let session = f.session();
            session.activate(&f.state).unwrap();
            let mut draft = Draft::new(pr());
            let mut saved = draft.clone();
            let attempt = crate::store::Attempt {
                payload: serde_json::json!({"event": "COMMENT"}),
                previous_review_ids: vec![],
            };
            let receipt = Receipt {
                id: 123,
                state: "COMMENTED".into(),
                url: "receipt".into(),
            };
            match case {
                "attempt" => draft.attempt = Some(attempt),
                "history" => draft.retry_history.push(attempt),
                "submitted" => draft.submitted = Some(receipt),
                "version" => draft.version += 1,
                "disk attempt" => saved.attempt = Some(attempt),
                "disk history" => saved.retry_history.push(attempt),
                "disk submitted" => saved.submitted = Some(receipt),
                "disk comments" => saved.comments.push(crate::model::Comment {
                    path: "src/example.rs".into(),
                    start_line: 1,
                    line: 1,
                    column: 1,
                    selected_text: "new".into(),
                    body: "newer draft".into(),
                }),
                "disk revision" => saved.pr.head_sha = "d".repeat(40),
                _ => saved.pr.number += 1,
            }
            f.store.save(&saved).unwrap();
            let bytes = fs::read(f.store.directory.join("draft.json")).unwrap();
            let error = cancel_review(&f.root, &f.store, &draft)
                .unwrap_err()
                .to_string();
            if case.contains("attempt") || case.contains("history") || case.contains("submitted") {
                assert!(error.contains("recover"), "{case}: {error}");
            }
            session.validate().unwrap();
            assert!(archives(&f.store).is_empty(), "{case}");
            assert!(!session.directory.join(CANCELLED_FILE).exists(), "{case}");
            assert_eq!(
                fs::read(f.store.directory.join("draft.json")).unwrap(),
                bytes
            );
            assert!(f.state.join("active.json").exists());
        }
    }

    #[test]
    fn cancellation_validates_every_candidate_and_active_pointer_before_archiving() {
        for case in [
            "content",
            "extra file",
            "extra directory",
            "missing",
            "traversal",
            "mapping",
            "identity",
            "invalid directory",
            "active json",
            "active version",
            "active digest",
            "active root",
            "active directory",
        ] {
            let f = Fixture::new();
            let first = f.session();
            let second = f.session();
            first.activate(&f.state).unwrap();
            let draft = Draft::new(pr());
            f.store.save(&draft).unwrap();
            match case {
                "content" => replace(&second.head_root().join("src/example.rs"), b"user edits"),
                "extra file" => fs::write(second.directory.join("notes.txt"), b"notes").unwrap(),
                "extra directory" => fs::create_dir(second.head_root().join("extra")).unwrap(),
                "missing" => fs::remove_file(second.head_root().join("src/example.rs")).unwrap(),
                "traversal" => {
                    change_manifest(&second, |m| m.files[0].filename = "../outside".into())
                }
                "mapping" => change_manifest(&second, |m| {
                    m.files[0].head.as_mut().unwrap().path = "head/other.rs".into()
                }),
                "identity" => change_manifest(&second, |m| m.pr.number += 1),
                "invalid directory" => {
                    fs::write(f.store.directory.join("diff-invalid"), b"notes").unwrap()
                }
                "active json" => fs::write(f.state.join("active.json"), b"not JSON").unwrap(),
                _ => {
                    let path = f.state.join("active.json");
                    let mut active: Active =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    match case {
                        "active version" => active.version += 1,
                        "active digest" => active.manifest_sha256 = "0".repeat(64),
                        "active root" => active.root = f.root.join("other"),
                        _ => active.directory = f.store.directory.join("../outside"),
                    }
                    fs::write(path, serde_json::to_vec(&active).unwrap()).unwrap();
                }
            }
            let active_bytes = fs::read(f.state.join("active.json")).unwrap();
            let draft_bytes = fs::read(f.store.directory.join("draft.json")).unwrap();
            assert!(cancel_review(&f.root, &f.store, &draft).is_err(), "{case}");
            first.validate().unwrap();
            assert!(second.directory.join("base/src/example.rs").exists());
            assert!(!second.directory.join(CANCELLED_FILE).exists());
            assert!(archives(&f.store).is_empty(), "{case}");
            assert_eq!(fs::read(f.state.join("active.json")).unwrap(), active_bytes);
            assert_eq!(
                fs::read(f.store.directory.join("draft.json")).unwrap(),
                draft_bytes
            );
            if case == "content" {
                assert_eq!(
                    fs::read(second.head_root().join("src/example.rs")).unwrap(),
                    b"user edits"
                );
            }
        }
    }

    #[test]
    fn cancelled_markers_never_relax_content_path_or_inventory_guards() {
        for case in [
            "version",
            "digest",
            "draft digest",
            "unknown",
            "manifest",
            "content",
            "extra",
            "directory",
            "mapping",
        ] {
            let f = Fixture::new();
            let session = f.session();
            let draft = Draft::new(pr());
            mark_cancelled(&f, &session, &draft);
            fs::remove_file(session.directory.join("base/src/example.rs")).unwrap();
            let path = session.directory.join(CANCELLED_FILE);
            let mut marker: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match case {
                "version" => marker["version"] = 99.into(),
                "digest" => marker["manifest_sha256"] = "0".repeat(64).into(),
                "draft digest" => marker["draft_sha256"] = "invalid".into(),
                "unknown" => marker["review_id"] = 123.into(),
                "manifest" => change_manifest(&session, |m| m.pr.title.push_str("changed")),
                "content" => replace(&session.head_root().join("src/example.rs"), b"user edits"),
                "extra" => fs::write(session.directory.join("notes"), b"notes").unwrap(),
                "directory" => fs::create_dir(session.directory.join("extra")).unwrap(),
                _ => {
                    change_manifest(&session, |m| {
                        m.files[0].head.as_mut().unwrap().path = "../outside".into()
                    });
                    marker["manifest_sha256"] =
                        digest(&fs::read(session.directory.join("manifest.json")).unwrap()).into();
                }
            }
            replace(&path, &serde_json::to_vec(&marker).unwrap());
            assert!(cancel_review(&f.root, &f.store, &draft).is_err(), "{case}");
            assert!(
                load_for_cancel(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .is_err(),
                "{case}"
            );
            assert!(session.head_root().join("src/example.rs").exists());
            assert_eq!(archives(&f.store).len(), 1);
            let submitted = f.submitted("COMMENTED");
            assert!(
                cleanup_submitted(&f.root, &f.store, &submitted).is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn cancel_loader_requires_exact_registered_head_or_base_paths() {
        let f = Fixture::new();
        let session = f.session();
        for cancelled in [false, true] {
            if cancelled {
                cancel_review(&f.root, &f.store, &Draft::new(pr())).unwrap();
            }
            for side in ["head", "base"] {
                assert!(
                    load_for_cancel(
                        &f.root,
                        &f.state,
                        &session.directory.join(side).join("src/example.rs")
                    )
                    .unwrap()
                    .is_some()
                );
            }
            for suffix in [
                "head/src/../src/example.rs",
                "head/src/./example.rs",
                "head/src//example.rs",
                "head/src/example.rs/",
                "head/src/EXAMPLE.rs",
                "head/src/unknown.rs",
                "base/src/unknown.rs",
                "head",
                "manifest.json",
                CANCELLED_FILE,
            ] {
                let path = PathBuf::from(format!("{}/{suffix}", session.directory.display()));
                assert!(
                    load_for_cancel(&f.root, &f.state, &path).is_err(),
                    "{suffix}"
                );
            }
            assert!(
                load_for_cancel(&f.root, &f.state, &f.root.join("ordinary.rs"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn cancellation_handles_empty_sessions_and_preserves_older_submission_receipts() {
        let f = Fixture::new();
        let old = f.with_files(&pr(), &[]);
        let submitted = f.submitted("APPROVED");
        cleanup_submitted(&f.root, &f.store, &submitted).unwrap();
        let receipt = fs::read(old.directory.join(CLEANUP_FILE)).unwrap();
        let archive = f.store.archive(&submitted).unwrap();
        let archived = fs::read(&archive).unwrap();
        f.store.clear().unwrap();
        let new = f.with_files(&pr(), &[]);
        let draft = Draft::new(pr());
        assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 2);
        assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 0);
        assert_eq!(fs::read(old.directory.join(CLEANUP_FILE)).unwrap(), receipt);
        assert_eq!(fs::read(archive).unwrap(), archived);
        assert_cancelled(old.validate());
        assert_cancelled(new.validate());
    }

    #[test]
    fn cancellation_rejects_external_or_inexact_stores() {
        let f = Fixture::new();
        let session = f.session();
        let external = tempfile::tempdir().unwrap();
        let mut store = Store::open(external.path(), &pr()).unwrap();
        fs::write(store.directory.join("draft.json"), b"user data").unwrap();
        assert!(cancel_review(&f.root, &store, &Draft::new(pr())).is_err());
        assert_eq!(
            fs::read(store.directory.join("draft.json")).unwrap(),
            b"user data"
        );
        store.directory = f.store.directory.join(".");
        assert!(cancel_review(&f.root, &store, &Draft::new(pr())).is_err());
        session.validate().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_rejects_symlinks_hardlinks_and_aliased_ancestors() {
        use std::os::unix::fs::symlink;
        for marked in [false, true] {
            for target in [
                "snapshot", "manifest", "head", "session", "key", "state", "scratch", "draft",
                "active", "marker",
            ] {
                if target == "marker" && !marked {
                    continue;
                }
                let f = Fixture::new();
                let session = f.session();
                session.activate(&f.state).unwrap();
                let draft = Draft::new(pr());
                f.store.save(&draft).unwrap();
                if marked {
                    mark_cancelled(&f, &session, &draft);
                }
                let path = match target {
                    "snapshot" => session.head_root().join("src/example.rs"),
                    "manifest" => session.directory.join("manifest.json"),
                    "head" => session.head_root(),
                    "session" => session.directory.clone(),
                    "key" => f.store.directory.clone(),
                    "state" => f.state.clone(),
                    "scratch" => f.root.join("scratch"),
                    "draft" => f.store.directory.join("draft.json"),
                    "active" => f.state.join("active.json"),
                    _ => session.directory.join(CANCELLED_FILE),
                };
                let moved = f.root.join("user-data");
                fs::rename(&path, &moved).unwrap();
                symlink(&moved, &path).unwrap();
                assert!(
                    cancel_review(&f.root, &f.store, &draft).is_err(),
                    "{target}, marked={marked}"
                );
                assert!(moved.exists());
                assert!(session.directory.join("base/src/example.rs").exists());
            }
            for target in [
                "head/src/example.rs",
                "manifest.json",
                CANCELLED_FILE,
                "draft",
                "active",
            ] {
                if target == CANCELLED_FILE && !marked {
                    continue;
                }
                let f = Fixture::new();
                let session = f.session();
                session.activate(&f.state).unwrap();
                let draft = Draft::new(pr());
                f.store.save(&draft).unwrap();
                if marked {
                    mark_cancelled(&f, &session, &draft);
                }
                let path = match target {
                    "draft" => f.store.directory.join("draft.json"),
                    "active" => f.state.join("active.json"),
                    _ => session.directory.join(target),
                };
                let link = f.root.join("hardlink");
                fs::hard_link(&path, &link).unwrap();
                let bytes = fs::read(&link).unwrap();
                assert!(
                    cancel_review(&f.root, &f.store, &draft).is_err(),
                    "{target}, marked={marked}"
                );
                assert_eq!(fs::read(&link).unwrap(), bytes);
                assert!(session.head_root().join("src/example.rs").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_alias_paths_never_fall_back() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let session = f.session();
        cancel_review(&f.root, &f.store, &Draft::new(pr())).unwrap();
        let alias = f.root.join("alias");
        symlink(&session.directory, &alias).unwrap();
        let dangling = f.root.join("dangling");
        symlink(session.head_root(), &dangling).unwrap();
        for path in [
            alias.join("head/src/example.rs"),
            dangling.join("src/example.rs"),
        ] {
            assert!(load_for_cancel(&f.root, &f.state, &path).is_err());
            assert!(load_for_navigation(&f.root, &f.state, &path).is_err());
        }
    }

    #[test]
    fn cleanup_approval_and_request_changes_keep_comments_receipts_and_metadata() {
        for state in ["COMMENTED", "APPROVED", "CHANGES_REQUESTED", "DISMISSED"] {
            let f = Fixture::new();
            let session = f.session();
            session.activate(&f.state).unwrap();
            let mut draft = f.submitted(state);
            draft.comments.push(crate::model::Comment {
                path: "src/example.rs".into(),
                start_line: 1,
                line: 1,
                column: 1,
                selected_text: "new".into(),
                body: "Please keep this comment".into(),
            });
            f.store.save(&draft).unwrap();
            let archive = f.store.archive(&draft).unwrap();
            let comments = f.store.directory.join("comments.md");
            let message = f.store.directory.join("message.txt");
            let receipt = f.store.directory.join("receipt.json");
            fs::write(&comments, b"Keep my comments\n").unwrap();
            fs::write(&message, b"Review summary\n").unwrap();
            fs::write(&receipt, b"Receipt\n").unwrap();
            let retained: Vec<_> = [
                comments,
                message,
                receipt,
                archive,
                f.store.directory.join("draft.json"),
                f.state.join("active.json"),
                session.directory.join("manifest.json"),
            ]
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
            for side in ["head", "base"] {
                assert!(
                    fs::metadata(session.directory.join(side).join("src/example.rs"))
                        .unwrap()
                        .permissions()
                        .readonly()
                );
            }
            assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 1);
            let marker = fs::read(session.directory.join(CLEANUP_FILE)).unwrap();
            assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 0);
            assert_eq!(
                fs::read(session.directory.join(CLEANUP_FILE)).unwrap(),
                marker
            );
            assert!(!session.head_root().exists());
            assert!(!session.directory.join("base").exists());
            assert_eq!(fs::read_dir(&session.directory).unwrap().count(), 2);
            for (path, bytes) in retained {
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn cleanup_requires_a_matching_successful_receipt_on_disk() {
        for case in [
            "no receipt",
            "uncertain",
            "zero id",
            "pending",
            "commented disk mismatch",
            "dismissed disk mismatch",
            "missing disk draft",
            "disk no receipt",
            "disk uncertain",
            "disk id",
            "disk state",
            "disk url",
            "disk revision",
            "disk reviewer",
            "version",
        ] {
            let f = Fixture::new();
            let session = f.session();
            let mut draft = f.submitted("APPROVED");
            let mut saved = draft.clone();
            let attempt = crate::store::Attempt {
                payload: serde_json::json!({"event": "APPROVE"}),
                previous_review_ids: vec![],
            };
            match case {
                "no receipt" => draft.submitted = None,
                "uncertain" => draft.attempt = Some(attempt),
                "zero id" => draft.submitted.as_mut().unwrap().id = 0,
                "pending" => draft.submitted.as_mut().unwrap().state = "PENDING".into(),
                "commented disk mismatch" => {
                    draft.submitted.as_mut().unwrap().state = "COMMENTED".into()
                }
                "dismissed disk mismatch" => {
                    draft.submitted.as_mut().unwrap().state = "DISMISSED".into()
                }
                "disk no receipt" => saved.submitted = None,
                "disk uncertain" => saved.attempt = Some(attempt),
                "disk id" => saved.submitted.as_mut().unwrap().id += 1,
                "disk state" => {
                    saved.submitted.as_mut().unwrap().state = "CHANGES_REQUESTED".into()
                }
                "disk url" => saved.submitted.as_mut().unwrap().url.push_str("-other"),
                "disk revision" => saved.pr.head_sha = "e".repeat(40),
                "disk reviewer" => saved.pr.reviewer = "another".into(),
                "version" => draft.version += 1,
                _ => {}
            }
            f.store.save(&saved).unwrap();
            if case == "missing disk draft" {
                fs::remove_file(f.store.directory.join("draft.json")).unwrap();
            }
            assert!(
                cleanup_submitted(&f.root, &f.store, &draft).is_err(),
                "{case}"
            );
            assert!(
                session.head_root().join("src/example.rs").exists(),
                "{case}"
            );
            assert!(!session.directory.join(CLEANUP_FILE).exists(), "{case}");
        }
    }

    #[test]
    fn cleanup_ignores_external_and_inexact_stores_without_loading_them() {
        let f = Fixture::new();
        let session = f.session();
        let draft = f.submitted("APPROVED");
        let external = tempfile::tempdir().unwrap();
        let mut store = Store::open(external.path(), &pr()).unwrap();
        fs::write(store.directory.join("draft.json"), b"invalid JSON").unwrap();
        fs::write(store.directory.join("diff-invalid"), b"user data").unwrap();
        assert_eq!(cleanup_submitted(&f.root, &store, &draft).unwrap(), 0);
        assert_eq!(
            fs::read(store.directory.join("diff-invalid")).unwrap(),
            b"user data"
        );
        store.directory = f.store.directory.join(".");
        assert_eq!(cleanup_submitted(&f.root, &store, &draft).unwrap(), 0);
        store.directory = external.path().join("does-not-exist");
        assert_eq!(cleanup_submitted(&f.root, &store, &draft).unwrap(), 0);
        assert!(session.validate().is_ok());
    }

    #[test]
    fn cleanup_covers_multiple_opens_and_old_revisions_but_not_other_prs_or_accounts() {
        let f = Fixture::new();
        let first = f.session();
        let second = f.session();
        let mut current = pr();
        current.head_sha = "d".repeat(40);
        current.base_sha = "e".repeat(40);
        let third = f.with_files(&current, &[changed("src/example.rs", "modified", None)]);
        let mut others = Vec::new();
        for field in ["number", "reviewer"] {
            let mut other = pr();
            if field == "number" {
                other.number += 1;
            } else {
                other.reviewer = "another".into();
            }
            let store = Store::open(&f.state, &other).unwrap();
            others.push(
                create_session(
                    &f.root,
                    &store,
                    &other,
                    &"c".repeat(40),
                    &[changed("src/example.rs", "modified", None)],
                    |_, _| Ok(b"other\n".to_vec()),
                )
                .unwrap(),
            );
        }
        others[0].activate(&f.state).unwrap();
        let active = fs::read(f.state.join("active.json")).unwrap();
        let mut draft = f.submitted("CHANGES_REQUESTED");
        draft.pr = current.clone();
        f.store.save(&draft).unwrap();
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 3);
        for session in [&first, &second, &third] {
            assert!(!session.head_root().exists());
            let loaded = load_for_navigation(
                &f.root,
                &f.state,
                &session.head_root().join("src/example.rs"),
            )
            .unwrap()
            .unwrap();
            assert_eq!(loaded.directory, session.directory);
            assert_eq!(loaded.pr, session.pr);
        }
        assert!(!first.pr.same_revision(&current));
        assert_eq!(fs::read(f.state.join("active.json")).unwrap(), active);
        for session in others {
            session.validate().unwrap();
            assert!(!session.directory.join(CLEANUP_FILE).exists());
        }
    }

    fn assert_cleaned_error<T: std::fmt::Debug>(result: Result<T>) {
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("snapshots") && error.contains("cleaned") && error.contains("reopen"),
            "{error}"
        );
    }

    #[test]
    fn partial_cleanup_resumes_and_preserves_context_without_allowing_comments_or_open() {
        let f = Fixture::new();
        let session = f.session();
        session.activate(&f.state).unwrap();
        let draft = f.submitted("APPROVED");
        write_cleanup(&session, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
        let marker = fs::read(session.directory.join(CLEANUP_FILE)).unwrap();
        let head = session.head_root().join("src/example.rs");
        fs::remove_file(&head).unwrap();
        fs::remove_dir(session.head_root().join("src")).unwrap();
        fs::remove_dir(session.head_root()).unwrap();
        for path in [&head, &session.directory.join("base/src/example.rs")] {
            let loaded = load_for_navigation(&f.root, &f.state, path)
                .unwrap()
                .unwrap();
            assert_eq!(loaded.pr, session.pr);
            assert_eq!(loaded.directory, session.directory);
            assert_cleaned_error(loaded.selection_path(&head));
            assert_cleaned_error(loaded.open());
        }
        assert_cleaned_error(load_for_file(&f.root, &f.state, &head));
        assert_cleaned_error(session.selection_path(&head));
        assert_cleaned_error(session.open());
        assert_eq!(
            load_active(&f.root, &f.state).unwrap().unwrap().directory,
            session.directory
        );
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 1);
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 0);
        assert_eq!(
            fs::read(session.directory.join(CLEANUP_FILE)).unwrap(),
            marker
        );
        // Draft archival must not remove context for an old diff or terminal.
        f.store.archive(&draft).unwrap();
        f.store.clear().unwrap();
        assert_eq!(
            load_active(&f.root, &f.state).unwrap().unwrap().pr,
            session.pr
        );
        for side in ["head", "base"] {
            let path = session.directory.join(side).join("src/example.rs");
            assert_eq!(
                load_for_navigation(&f.root, &f.state, &path)
                    .unwrap()
                    .unwrap()
                    .directory,
                session.directory
            );
        }
    }

    #[test]
    fn later_review_resumes_partial_cleanup_after_the_earlier_draft_is_discarded() {
        let f = Fixture::new();
        let first = f.session();
        let first_draft = f.submitted("APPROVED");
        write_cleanup(&first, &f.store, first_draft.submitted.as_ref().unwrap()).unwrap();
        let first_marker = fs::read(first.directory.join(CLEANUP_FILE)).unwrap();
        fs::remove_file(first.head_root().join("src/example.rs")).unwrap();
        fs::remove_dir(first.head_root().join("src")).unwrap();
        fs::remove_dir(first.head_root()).unwrap();
        assert!(first.directory.join("base/src/example.rs").is_file());

        let archive = f.store.archive(&first_draft).unwrap();
        let archived_bytes = fs::read(&archive).unwrap();
        f.store.clear().unwrap();
        let mut next_pr = pr();
        next_pr.head_sha = "d".repeat(40);
        let mut next_draft = Draft::new(next_pr.clone());
        next_draft.submitted = Some(Receipt {
            id: 456,
            url: format!("{}#pullrequestreview-456", next_pr.url),
            state: "CHANGES_REQUESTED".into(),
        });
        f.store.save(&next_draft).unwrap();
        let current_bytes = fs::read(f.store.directory.join("draft.json")).unwrap();
        let second = f.with_files(&next_pr, &[changed("src/example.rs", "modified", None)]);

        assert_eq!(
            cleanup_submitted(&f.root, &f.store, &next_draft).unwrap(),
            2
        );
        assert_eq!(
            cleanup_submitted(&f.root, &f.store, &next_draft).unwrap(),
            0
        );
        for session in [&first, &second] {
            assert!(!session.head_root().exists());
            assert!(!session.directory.join("base").exists());
            assert_eq!(
                load_for_navigation(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .unwrap()
                .unwrap()
                .pr,
                session.pr
            );
        }
        assert_eq!(
            fs::read(first.directory.join(CLEANUP_FILE)).unwrap(),
            first_marker
        );
        assert_eq!(fs::read(&archive).unwrap(), archived_bytes);
        assert_eq!(
            fs::read(f.store.directory.join("draft.json")).unwrap(),
            current_bytes
        );
        let second_marker: Cleanup =
            serde_json::from_slice(&fs::read(second.directory.join(CLEANUP_FILE)).unwrap())
                .unwrap();
        assert_eq!(second_marker.review_id, 456);
        assert_eq!(second_marker.review_state, "CHANGES_REQUESTED");
    }

    #[test]
    fn cleanup_resumes_at_each_deletion_boundary() {
        for removed in 0..=6 {
            let f = Fixture::new();
            let session = f.session();
            let draft = f.submitted("APPROVED");
            write_cleanup(&session, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
            let paths = [
                "base/src/example.rs",
                "head/src/example.rs",
                "base/src",
                "head/src",
                "base",
                "head",
            ];
            for (index, relative) in paths[..removed].iter().enumerate() {
                let path = session.directory.join(relative);
                if index < 2 {
                    fs::remove_file(path).unwrap();
                } else {
                    fs::remove_dir(path).unwrap();
                }
            }
            for side in ["base", "head"] {
                assert!(
                    load_for_navigation(
                        &f.root,
                        &f.state,
                        &session.directory.join(side).join("src/example.rs")
                    )
                    .unwrap()
                    .is_some()
                );
            }
            assert_eq!(
                cleanup_submitted(&f.root, &f.store, &draft).unwrap(),
                usize::from(removed < 6)
            );
            assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 0);
        }
    }

    #[test]
    fn cleaned_tabs_never_rebind_and_only_exact_registered_paths_load() {
        let f = Fixture::new();
        let first = f.session();
        let draft = f.submitted("APPROVED");
        cleanup_submitted(&f.root, &f.store, &draft).unwrap();
        let mut newer = pr();
        newer.head_sha = "e".repeat(40);
        let second = f.with_files(&newer, &[changed("src/example.rs", "modified", None)]);
        second.activate(&f.state).unwrap();
        let old = load_for_navigation(&f.root, &f.state, &first.head_root().join("src/example.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(old.directory, first.directory);
        assert!(!old.pr.same_revision(&newer));
        assert_eq!(
            load_active(&f.root, &f.state).unwrap().unwrap().directory,
            second.directory
        );
        for suffix in [
            "head/src/../src/example.rs",
            "head/src/./example.rs",
            "head/src//example.rs",
            "head/src/example.rs/",
            "head/src/EXAMPLE.rs",
            "head/src/unregistered.rs",
            "base/src/unregistered.rs",
            "head",
            "manifest.json",
            "cleanup.json",
        ] {
            let path = PathBuf::from(format!("{}/{suffix}", first.directory.display()));
            assert!(
                load_for_navigation(&f.root, &f.state, &path).is_err(),
                "{suffix}"
            );
        }
        assert!(
            load_for_navigation(
                &f.root,
                &f.state,
                &f.store.directory.join("diff-missing/head/src/example.rs")
            )
            .is_err()
        );
        assert!(
            load_for_navigation(&f.root, &f.state, &f.root.join("ordinary.rs"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cleanup_validates_all_sessions_before_deleting_any_files() {
        for case in [
            "modified",
            "extra file",
            "extra directory",
            "traversal",
            "missing",
            "invalid session",
        ] {
            let f = Fixture::new();
            let first = f.session();
            let second = f.session();
            let draft = f.submitted("APPROVED");
            match case {
                "modified" => replace(&second.head_root().join("src/example.rs"), b"user edits"),
                "extra file" => {
                    fs::write(second.directory.join("notes.txt"), b"user notes").unwrap()
                }
                "extra directory" => fs::create_dir(second.head_root().join("extra")).unwrap(),
                "traversal" => {
                    change_manifest(&second, |m| m.files[0].filename = "../outside".into())
                }
                "missing" => fs::remove_file(second.head_root().join("src/example.rs")).unwrap(),
                _ => fs::write(f.store.directory.join("diff-invalid"), b"user file").unwrap(),
            }
            assert!(
                cleanup_submitted(&f.root, &f.store, &draft).is_err(),
                "{case}"
            );
            first.validate().unwrap();
            assert!(second.directory.join("base/src/example.rs").is_file());
            assert!(!first.directory.join(CLEANUP_FILE).exists());
            assert!(!second.directory.join(CLEANUP_FILE).exists());
            if case == "modified" {
                assert_eq!(
                    fs::read(second.head_root().join("src/example.rs")).unwrap(),
                    b"user edits"
                );
            }
            if case == "extra file" {
                assert_eq!(
                    fs::read(second.directory.join("notes.txt")).unwrap(),
                    b"user notes"
                );
            }
        }
    }

    #[test]
    fn cleanup_marker_cannot_hide_modified_or_unregistered_remaining_files() {
        for case in [
            "modified",
            "extra file",
            "extra directory",
            "manifest",
            "marker version",
            "marker digest",
            "marker id",
            "marker state",
            "marker unknown",
        ] {
            let f = Fixture::new();
            let session = f.session();
            session.activate(&f.state).unwrap();
            let draft = f.submitted("APPROVED");
            write_cleanup(&session, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
            let marker_path = session.directory.join(CLEANUP_FILE);
            let mut marker: serde_json::Value =
                serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
            match case {
                "modified" => replace(&session.head_root().join("src/example.rs"), b"user edits"),
                "extra file" => {
                    fs::write(session.directory.join("notes.txt"), b"user notes").unwrap()
                }
                "extra directory" => fs::create_dir(session.directory.join("extra")).unwrap(),
                "manifest" => change_manifest(&session, |m| m.pr.title.push_str(" changed")),
                "marker version" => marker["version"] = 99.into(),
                "marker digest" => marker["manifest_sha256"] = "0".repeat(64).into(),
                "marker id" => marker["review_id"] = 0.into(),
                "marker state" => marker["review_state"] = "PENDING".into(),

                _ => marker["unknown"] = true.into(),
            }
            replace(&marker_path, &serde_json::to_vec(&marker).unwrap());
            let head = session.head_root().join("src/example.rs");
            assert!(
                cleanup_submitted(&f.root, &f.store, &draft).is_err(),
                "{case}"
            );
            assert!(head.exists(), "{case}");
            assert!(
                load_for_navigation(&f.root, &f.state, &head).is_err(),
                "{case}"
            );
            assert!(load_active(&f.root, &f.state).is_err(), "{case}");
            assert!(session.selection_path(&head).is_err());
        }
    }

    #[test]
    fn marked_manifests_still_require_safe_mapping_and_identity() {
        for field in [
            "mapping",
            "traversal",
            "root",
            "directory",
            "reviewer",
            "membership",
            "digest",
            "file directory collision",
        ] {
            let f = Fixture::new();
            let session = f.session();
            let draft = f.submitted("APPROVED");
            cleanup_submitted(&f.root, &f.store, &draft).unwrap();
            change_manifest(&session, |m| match field {
                "mapping" => m.files[0].head.as_mut().unwrap().repository_path = "other.rs".into(),
                "traversal" => m.files[0].filename = "../other.rs".into(),
                "root" => m.root = f.root.join("other"),
                "directory" => m.directory = f.store.directory.join("diff-other"),
                "reviewer" => m.pr.reviewer = "other".into(),
                "membership" => m.files[0].head = None,
                "digest" => m.files[0].head.as_mut().unwrap().sha256 = "invalid".into(),
                _ => {
                    let mut file = m.files[0].clone();
                    file.filename = "src/example.rs/nested.rs".into();
                    for (side, snapshot) in [("head", &mut file.head), ("base", &mut file.base)] {
                        let snapshot = snapshot.as_mut().unwrap();
                        snapshot.repository_path = file.filename.clone();
                        snapshot.path = format!("{side}/{}", file.filename);
                    }
                    m.files.push(file);
                }
            });
            // Even a matching digest cannot waive structural manifest checks.
            let path = session.directory.join(CLEANUP_FILE);
            let mut marker: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            marker["manifest_sha256"] =
                digest(&fs::read(session.directory.join("manifest.json")).unwrap()).into();
            replace(&path, &serde_json::to_vec(&marker).unwrap());
            assert!(
                load_for_navigation(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .is_err(),
                "{field}"
            );
            assert!(
                cleanup_submitted(&f.root, &f.store, &draft).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn empty_and_binary_sessions_support_interruption_and_later_reviews() {
        let f = Fixture::new();
        let empty = f.with_files(&pr(), &[]);
        let binary = create_session(
            &f.root,
            &f.store,
            &pr(),
            &"c".repeat(40),
            &[changed("image.bin", "modified", None)],
            |sha, _| {
                Ok(if sha == pr().head_sha {
                    b"text".to_vec()
                } else {
                    b"\0binary".to_vec()
                })
            },
        )
        .unwrap();
        let draft = f.submitted("APPROVED");
        write_cleanup(&binary, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
        fs::remove_file(binary.directory.join("base/image.bin")).unwrap();
        assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 2);
        assert_cleaned_error(empty.open());
        let later = f.session();
        let old_marker = fs::read(binary.directory.join(CLEANUP_FILE)).unwrap();
        let mut next_draft = f.submitted("CHANGES_REQUESTED");
        next_draft.submitted.as_mut().unwrap().id += 1;
        f.store.save(&next_draft).unwrap();
        assert_eq!(
            cleanup_submitted(&f.root, &f.store, &next_draft).unwrap(),
            1
        );
        assert!(!later.head_root().exists());
        assert_eq!(
            fs::read(binary.directory.join(CLEANUP_FILE)).unwrap(),
            old_marker
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_rejects_symlinks_hardlinks_and_aliased_ancestors() {
        use std::os::unix::fs::symlink;
        for marked in [false, true] {
            for target in [
                "snapshot", "manifest", "head", "session", "key", "state", "scratch", "draft",
                "marker",
            ] {
                if target == "marker" && !marked {
                    continue;
                }
                let f = Fixture::new();
                let session = f.session();
                let draft = f.submitted("APPROVED");
                if marked {
                    write_cleanup(&session, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
                }
                let path = match target {
                    "snapshot" => session.head_root().join("src/example.rs"),
                    "manifest" => session.directory.join("manifest.json"),
                    "head" => session.head_root(),
                    "session" => session.directory.clone(),
                    "key" => f.store.directory.clone(),
                    "state" => f.state.clone(),
                    "scratch" => f.root.join("scratch"),
                    "draft" => f.store.directory.join("draft.json"),
                    _ => session.directory.join(CLEANUP_FILE),
                };
                let moved = f.root.join("user-data");
                fs::rename(&path, &moved).unwrap();
                symlink(&moved, &path).unwrap();
                assert!(
                    cleanup_submitted(&f.root, &f.store, &draft).is_err(),
                    "{target}, marked={marked}"
                );
                assert!(moved.exists());
                assert!(session.directory.join("base/src/example.rs").exists());
            }
            for target in [
                "head/src/example.rs",
                "manifest.json",
                CLEANUP_FILE,
                "draft",
            ] {
                if target == CLEANUP_FILE && !marked {
                    continue;
                }
                let f = Fixture::new();
                let session = f.session();
                let draft = f.submitted("APPROVED");
                if marked {
                    write_cleanup(&session, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
                }
                let path = if target == "draft" {
                    f.store.directory.join("draft.json")
                } else {
                    session.directory.join(target)
                };
                let link = f.root.join("hardlink");
                fs::hard_link(&path, &link).unwrap();
                let bytes = fs::read(&link).unwrap();
                assert!(
                    cleanup_submitted(&f.root, &f.store, &draft).is_err(),
                    "{target}, marked={marked}"
                );
                assert_eq!(fs::read(&link).unwrap(), bytes);
                assert!(session.head_root().join("src/example.rs").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_cleaned_paths_through_root_aliases_never_fall_back() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let session = f.session();
        let draft = f.submitted("APPROVED");
        cleanup_submitted(&f.root, &f.store, &draft).unwrap();
        let alias = f.root.join("alias");
        symlink(&session.directory, &alias).unwrap();
        assert!(
            load_for_navigation(&f.root, &f.state, &alias.join("head/src/example.rs")).is_err()
        );
        let dangling = f.root.join("dangling");
        symlink(session.head_root().join("src/example.rs"), &dangling).unwrap();
        assert!(load_for_navigation(&f.root, &f.state, &dangling).is_err());
        let missing_parent = f.root.join("missing-parent");
        symlink(session.head_root(), &missing_parent).unwrap();
        assert!(
            load_for_navigation(&f.root, &f.state, &missing_parent.join("src/example.rs")).is_err()
        );
        let marker = session.directory.join(CLEANUP_FILE);
        fs::remove_file(&marker).unwrap();
        assert!(
            load_for_navigation(
                &f.root,
                &f.state,
                &session.head_root().join("src/example.rs")
            )
            .is_err()
        );
    }

    fn staging_directories(store: &Store) -> Vec<PathBuf> {
        fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(STAGING_PREFIX)
            })
            .collect()
    }

    #[test]
    fn snapshots_are_staged_then_published_with_final_manifest_binding() {
        let f = Fixture::new();
        let mut stage = None;
        let mut reads = 0;
        let session = create_session(
            &f.root,
            &f.store,
            &pr(),
            &"c".repeat(40),
            &[changed("src/example.rs", "modified", None)],
            |_, _| {
                reads += 1;
                let stages = staging_directories(&f.store);
                assert_eq!(stages.len(), 1);
                assert!(
                    load_store_sessions(&f.root, &f.state, &f.store.directory)
                        .unwrap()
                        .is_empty()
                );
                assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
                assert!(load_directory(&f.root, &f.state, &stages[0]).is_err());
                if reads == 2 {
                    assert_eq!(
                        fs::read(stages[0].join("base/src/example.rs")).unwrap(),
                        b"new\n"
                    );
                }
                stage = Some(stages[0].clone());
                Ok(b"new\n".to_vec())
            },
        )
        .unwrap();
        assert_eq!(reads, 2);
        let stage = stage.unwrap();
        assert!(!stage.exists());
        assert!(staging_directories(&f.store).is_empty());
        let suffix = stage
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix(STAGING_PREFIX)
            .unwrap();
        assert_eq!(
            session.directory,
            f.store.directory.join(format!("diff-{suffix}"))
        );
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(session.directory.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.directory, session.directory);
        assert_eq!(manifest, session.manifest);
        session.validate().unwrap();
        session.remember(&f.store).unwrap();
        let cached = Session::prepared(&f.store, &pr()).unwrap().unwrap();
        assert_eq!(cached.directory, session.directory);
        assert_eq!(cached.manifest_sha256, session.manifest_sha256);
        assert_eq!(
            cached
                .selection_path(&cached.head_root().join("src/example.rs"))
                .unwrap(),
            "src/example.rs"
        );
        assert_eq!(
            load_store_sessions(&f.root, &f.state, &f.store.directory)
                .unwrap()
                .len(),
            1
        );
        assert!(!f.state.join("active.json").exists());
        assert!(f.store.load().unwrap().is_none());
    }

    #[test]
    fn staging_is_removed_on_blob_or_full_validation_failure() {
        for failure in ["first_blob", "second_blob", "validation"] {
            let f = Fixture::new();
            let before: BTreeSet<_> = fs::read_dir(&f.store.directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            let mut reads = 0;
            let result = create_session(
                &f.root,
                &f.store,
                &pr(),
                &"c".repeat(40),
                &[changed("src/example.rs", "modified", None)],
                |_, _| {
                    reads += 1;
                    assert!(
                        load_store_sessions(&f.root, &f.state, &f.store.directory)
                            .unwrap()
                            .is_empty()
                    );
                    if failure == "first_blob" || (failure == "second_blob" && reads == 2) {
                        bail!("Interrupted blob read");
                    }
                    if failure == "validation" && reads == 2 {
                        let stages = staging_directories(&f.store);
                        replace(&stages[0].join("base/src/example.rs"), b"changed\n");
                    }
                    Ok(b"new\n".to_vec())
                },
            );
            assert!(result.is_err(), "{failure}");
            assert!(staging_directories(&f.store).is_empty(), "{failure}");
            let after: BTreeSet<_> = fs::read_dir(&f.store.directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(after, before, "{failure}");
            assert!(
                load_store_sessions(&f.root, &f.state, &f.store.directory)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn staging_publication_never_replaces_an_existing_destination() {
        let f = Fixture::new();
        let mut destination = None;
        let result = create_session(
            &f.root,
            &f.store,
            &pr(),
            &"c".repeat(40),
            &[changed("example.rs", "added", None)],
            |_, _| {
                let stages = staging_directories(&f.store);
                let suffix = stages[0]
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix(STAGING_PREFIX)
                    .unwrap();
                let path = f.store.directory.join(format!("diff-{suffix}"));
                // Even an empty existing directory must not be overwritten by rename.
                fs::create_dir(&path).unwrap();
                destination = Some(path);
                Ok(b"new\n".to_vec())
            },
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("destination already exists")
        );
        let destination = destination.unwrap();
        assert!(destination.is_dir());
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
        assert!(staging_directories(&f.store).is_empty());
    }

    #[test]
    fn interrupted_staging_directories_do_not_block_cancellation_or_submitted_cleanup() {
        for cancel in [false, true] {
            let f = Fixture::new();
            let session = f.session();
            session.remember(&f.store).unwrap();
            session.activate(&f.state).unwrap();
            let mut stages = Vec::new();
            for phase in ["empty", "base", "manifest", "complete"] {
                // Model process termination at several write boundaries by retaining
                // staging directories instead of allowing TempDir to remove them.
                let staging = tempfile::Builder::new()
                    .prefix(STAGING_PREFIX)
                    .tempdir_in(&f.store.directory)
                    .unwrap()
                    .keep();
                if phase != "empty" {
                    fs::create_dir(staging.join("base")).unwrap();
                    fs::create_dir(staging.join("head")).unwrap();
                    write_snapshot(&f.root, &staging, "base/src/example.rs", b"old\n").unwrap();
                }
                if phase == "manifest" {
                    fs::write(staging.join("manifest.json"), b"{partial manifest").unwrap();
                }
                if phase == "complete" {
                    write_snapshot(&f.root, &staging, "head/src/example.rs", b"new\n").unwrap();
                    let suffix = staging
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .strip_prefix(STAGING_PREFIX)
                        .unwrap();
                    let mut manifest = session.manifest.clone();
                    manifest.directory = f.store.directory.join(format!("diff-{suffix}"));
                    write_snapshot(
                        &f.root,
                        &staging,
                        "manifest.json",
                        &serde_json::to_vec(&manifest).unwrap(),
                    )
                    .unwrap();
                    load_directory_from(&f.root, &f.state, &manifest.directory, &staging).unwrap();
                }
                assert!(load_directory(&f.root, &f.state, &staging).is_err());
                let mut files = BTreeSet::new();
                let mut directories = BTreeSet::new();
                inventory(&f.root, &staging, &staging, &mut files, &mut directories).unwrap();
                let bytes: Vec<_> = files
                    .into_iter()
                    .map(|file| {
                        let bytes = fs::read(staging.join(&file)).unwrap();
                        (file, bytes)
                    })
                    .collect();
                stages.push((staging, directories, bytes));
            }
            assert_eq!(
                load_store_sessions(&f.root, &f.state, &f.store.directory)
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                Session::prepared(&f.store, &pr())
                    .unwrap()
                    .unwrap()
                    .directory,
                session.directory
            );
            if cancel {
                let draft = Draft::new(pr());
                f.store.save(&draft).unwrap();
                assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 1);
                assert_eq!(cancel_review(&f.root, &f.store, &draft).unwrap(), 0);
            } else {
                let draft = f.submitted("APPROVED");
                assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 1);
                assert_eq!(cleanup_submitted(&f.root, &f.store, &draft).unwrap(), 0);
            }
            assert!(!session.head_root().exists());
            assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
            assert_eq!(
                load_store_sessions(&f.root, &f.state, &f.store.directory)
                    .unwrap()
                    .len(),
                1
            );
            for (staging, original_dirs, bytes) in stages {
                let mut files = BTreeSet::new();
                let mut directories = BTreeSet::new();
                inventory(&f.root, &staging, &staging, &mut files, &mut directories).unwrap();
                assert_eq!(directories, original_dirs);
                assert_eq!(files, bytes.iter().map(|(file, _)| file.clone()).collect());
                for (file, original) in bytes {
                    assert_eq!(fs::read(staging.join(file)).unwrap(), original);
                }
            }
        }
    }

    #[test]
    fn cached_patches_preserve_absent_empty_and_renamed_files() {
        let f = Fixture::new();
        let mut files = vec![
            changed("src/example.rs", "modified", None),
            changed("new.rs", "renamed", Some("old.rs")),
            changed("empty.rs", "added", None),
        ];
        files[1].patch = None;
        files[2].patch = Some(" \n".into());
        let session = f.with_files(&pr(), &files);
        let loaded = load_directory(&f.root, &f.state, &session.directory).unwrap();
        assert_eq!(loaded.comment_files(), Some(files));
        assert!(!loaded.manifest.files[1].has_patch);
        assert!(!loaded.manifest.files[2].has_patch);
        let empty = f.with_files(&pr(), &[]);
        assert_eq!(empty.comment_files(), Some(vec![]));
        empty.remember(&f.store).unwrap();
        assert_eq!(
            Session::prepared(&f.store, &pr())
                .unwrap()
                .unwrap()
                .directory,
            empty.directory
        );
    }

    #[test]
    fn legacy_manifests_load_but_require_patch_fallback() {
        for files in [vec![], vec![changed("src/example.rs", "modified", None)]] {
            let f = Fixture::new();
            let session = f.with_files(&pr(), &files);
            let path = session.directory.join("manifest.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            manifest.as_object_mut().unwrap().remove("patches_cached");
            for file in manifest["files"].as_array_mut().unwrap() {
                file.as_object_mut().unwrap().remove("patch");
            }
            replace(&path, &serde_json::to_vec(&manifest).unwrap());
            let loaded = load_directory(&f.root, &f.state, &session.directory).unwrap();
            assert!(loaded.comment_files().is_none());
            loaded.remember(&f.store).unwrap();
            let cached = Session::prepared(&f.store, &pr()).unwrap().unwrap();
            assert!(cached.comment_files().is_none());
            if !files.is_empty() {
                let (path, bytes, files) = cached
                    .comment_data(&cached.head_root().join("src/example.rs"))
                    .unwrap();
                assert_eq!(path, "src/example.rs");
                assert_eq!(bytes, b"new\n");
                assert!(files.is_none());
            }
        }
    }

    #[test]
    fn cached_patch_flag_and_has_patch_must_agree() {
        for field in ["patch", "has_patch", "patches_cached"] {
            let f = Fixture::new();
            let session = f.session();
            change_manifest(&session, |manifest| match field {
                "patch" => manifest.files[0].patch = None,
                "has_patch" => manifest.files[0].has_patch = false,
                _ => manifest.patches_cached = false,
            });
            assert!(load_directory(&f.root, &f.state, &session.directory).is_err());
        }
    }

    #[test]
    fn prepared_reuses_only_remembered_sessions_without_changing_active_or_draft() {
        let f = Fixture::new();
        assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
        let first = f.session();
        assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
        first.remember(&f.store).unwrap();
        assert!(!f.state.join("active.json").exists());
        assert!(f.store.load().unwrap().is_none());
        first.activate(&f.state).unwrap();
        f.store.save(&Draft::new(pr())).unwrap();
        let active = fs::read(f.state.join("active.json")).unwrap();
        let draft = fs::read(f.store.directory.join("draft.json")).unwrap();
        let second = f.session();
        assert_ne!(first.directory, second.directory);
        for expected in [&first, &second] {
            expected.remember(&f.store).unwrap();
            let pointer = fs::read(f.store.directory.join(PREPARED_FILE)).unwrap();
            let mut current = pr();
            current.title = "A title change is not a new revision".into();
            let cached = Session::prepared(&f.store, &current).unwrap().unwrap();
            assert_eq!(cached.directory, expected.directory);
            assert_eq!(cached.manifest_sha256, expected.manifest_sha256);
            assert_eq!(
                fs::read(f.store.directory.join(PREPARED_FILE)).unwrap(),
                pointer
            );
            assert_eq!(fs::read(f.state.join("active.json")).unwrap(), active);
            assert_eq!(
                fs::read(f.store.directory.join("draft.json")).unwrap(),
                draft
            );
        }
        assert!(archives(&f.store).is_empty());
        assert_eq!(
            load_store_sessions(&f.root, &f.state, &f.store.directory)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn prepared_requires_same_revision_and_store() {
        let f = Fixture::new();
        let session = f.session();
        session.remember(&f.store).unwrap();
        for field in [
            "host",
            "repo",
            "number",
            "head",
            "base",
            "head_repo",
            "head_ref",
            "reviewer",
        ] {
            let mut current = pr();
            match field {
                "host" => current.host = "elsewhere.example".into(),
                "repo" => current.repo = "other/repo".into(),
                "number" => current.number += 1,
                "head" => current.head_sha = "d".repeat(40),
                "base" => current.base_sha = "e".repeat(40),
                "head_repo" => current.head_repo = "other/fork".into(),
                "head_ref" => current.head_ref = "other-topic".into(),
                _ => current.reviewer = "another".into(),
            }
            assert!(
                Session::prepared(&f.store, &current).unwrap().is_none(),
                "{field}"
            );
        }
        let mut other = pr();
        other.number += 1;
        let other_store = Store::open(&f.state, &other).unwrap();
        assert!(session.remember(&other_store).is_err());
        fs::copy(
            f.store.directory.join(PREPARED_FILE),
            other_store.directory.join(PREPARED_FILE),
        )
        .unwrap();
        assert!(Session::prepared(&other_store, &other).unwrap().is_none());
        assert!(Session::prepared(&other_store, &pr()).unwrap().is_none());
    }

    #[test]
    fn corrupt_prepared_pointers_are_read_only_misses_and_can_be_replaced() {
        let f = Fixture::new();
        let session = f.session();
        session.remember(&f.store).unwrap();
        let path = f.store.directory.join(PREPARED_FILE);
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for field in [
            "version",
            "root",
            "directory",
            "alias",
            "manifest_sha256",
            "unknown",
            "json",
        ] {
            let mut pointer = original.clone();
            match field {
                "version" => pointer["version"] = 99.into(),
                "root" => pointer["root"] = f.root.join("other").to_str().unwrap().into(),
                "directory" => {
                    pointer["directory"] = f
                        .store
                        .directory
                        .join("diff-missing")
                        .to_str()
                        .unwrap()
                        .into()
                }
                "alias" => {
                    pointer["directory"] = format!("{}/head/..", session.directory.display()).into()
                }
                "manifest_sha256" => pointer["manifest_sha256"] = "0".repeat(64).into(),
                _ => pointer["unknown"] = true.into(),
            }
            let bytes = if field == "json" {
                b"{broken JSON".to_vec()
            } else {
                serde_json::to_vec(&pointer).unwrap()
            };
            fs::write(&path, &bytes).unwrap();
            assert!(
                Session::prepared(&f.store, &pr()).unwrap().is_none(),
                "{field}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        session.remember(&f.store).unwrap();
        assert!(Session::prepared(&f.store, &pr()).unwrap().is_some());
        // An unrelated corrupt active pointer cannot invalidate a prepared hit.
        fs::write(f.state.join("active.json"), b"{broken JSON").unwrap();
        assert!(Session::prepared(&f.store, &pr()).unwrap().is_some());
        fs::remove_file(&path).unwrap();
        assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
    }

    #[test]
    fn prepared_validates_the_whole_session_before_reuse() {
        for damage in [
            "base",
            "head",
            "missing",
            "manifest",
            "missing_manifest",
            "directory",
            "extra",
        ] {
            let f = Fixture::new();
            let session = f.session();
            session.remember(&f.store).unwrap();
            match damage {
                "base" | "head" => replace(
                    &session.directory.join(damage).join("src/example.rs"),
                    b"edited\n",
                ),
                "missing" => fs::remove_file(session.head_root().join("src/example.rs")).unwrap(),
                "manifest" => {
                    change_manifest(&session, |manifest| manifest.pr.title = "Changed".into())
                }
                "missing_manifest" => {
                    fs::remove_file(session.directory.join("manifest.json")).unwrap()
                }
                "directory" => fs::remove_dir_all(&session.directory).unwrap(),
                _ => fs::write(session.head_root().join("unregistered.rs"), b"new\n").unwrap(),
            }
            assert!(
                Session::prepared(&f.store, &pr()).unwrap().is_none(),
                "{damage}"
            );
            assert!(session.remember(&f.store).is_err(), "{damage}");
        }
    }

    #[test]
    fn prepared_does_not_reuse_cancelled_or_cleaned_sessions_even_before_deletion() {
        for cancel in [false, true] {
            let f = Fixture::new();
            let session = f.session();
            session.remember(&f.store).unwrap();
            let draft = if cancel {
                Draft::new(pr())
            } else {
                f.submitted("APPROVED")
            };
            if cancel {
                mark_cancelled(&f, &session, &draft);
            } else {
                write_cleanup(&session, &f.store, draft.submitted.as_ref().unwrap()).unwrap();
            }
            assert!(session.head_root().join("src/example.rs").is_file());
            assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
            assert!(session.remember(&f.store).is_err());
            if cancel {
                cancel_review(&f.root, &f.store, &draft).unwrap();
            } else {
                cleanup_submitted(&f.root, &f.store, &draft).unwrap();
            }
            assert!(!session.head_root().exists());
            assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
            let fresh = f.session();
            fresh.remember(&f.store).unwrap();
            assert_eq!(
                Session::prepared(&f.store, &pr())
                    .unwrap()
                    .unwrap()
                    .directory,
                fresh.directory
            );
        }
    }

    #[test]
    fn local_comment_data_checks_selected_bytes_manifest_and_liveness() {
        for damage in [
            "content",
            "missing",
            "manifest",
            "missing_manifest",
            "cancelled",
            "cleaned",
            "identity",
        ] {
            let f = Fixture::new();
            let session = f.session();
            let file = session.head_root().join("src/example.rs");
            let mut loaded = load_for_file(&f.root, &f.state, &file).unwrap().unwrap();
            assert_eq!(
                loaded.comment_data(&file).unwrap(),
                (
                    "src/example.rs".into(),
                    b"new\n".to_vec(),
                    loaded.comment_files()
                )
            );
            match damage {
                "content" => replace(&file, b"edited\n"),
                "missing" => fs::remove_file(&file).unwrap(),
                "manifest" => change_manifest(&session, |manifest| manifest.files[0].patch = None),
                "missing_manifest" => {
                    fs::remove_file(session.directory.join("manifest.json")).unwrap()
                }
                // Even invalid markers cannot authorize using a loaded live session.
                "cancelled" => fs::write(session.directory.join(CANCELLED_FILE), b"{}").unwrap(),
                "cleaned" => fs::write(session.directory.join(CLEANUP_FILE), b"{}").unwrap(),
                _ => loaded.pr.head_sha = "d".repeat(40),
            }
            assert!(loaded.comment_data(&file).is_err(), "{damage}");
        }
    }

    #[test]
    fn local_comment_data_requires_exact_registered_rhs_mapping() {
        let f = Fixture::new();
        let session = f.session();
        let ordinary = f.root.join("ordinary.rs");
        fs::write(&ordinary, b"new\n").unwrap();
        for path in [
            ordinary,
            session.directory.join("base/src/example.rs"),
            session.directory.join("manifest.json"),
            session.head_root().join("missing.rs"),
            PathBuf::from(format!(
                "{}/head/./src/example.rs",
                session.directory.display()
            )),
            PathBuf::from(format!(
                "{}/head//src/example.rs",
                session.directory.display()
            )),
        ] {
            assert!(session.comment_data(&path).is_err(), "{}", path.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepared_and_local_comment_data_reject_symlinks_and_hardlinks() {
        use std::os::unix::fs::symlink;

        for pointer in [false, true] {
            for hardlink in [false, true] {
                let f = Fixture::new();
                let session = f.session();
                session.remember(&f.store).unwrap();
                let file = session.head_root().join("src/example.rs");
                let path = if pointer {
                    f.store.directory.join(PREPARED_FILE)
                } else {
                    file.clone()
                };
                let source = f.root.join("linked-source");
                fs::copy(&path, &source).unwrap();
                fs::remove_file(&path).unwrap();
                if hardlink {
                    fs::hard_link(&source, &path).unwrap();
                } else {
                    symlink(&source, &path).unwrap();
                }
                assert!(Session::prepared(&f.store, &pr()).unwrap().is_none());
                assert!(session.remember(&f.store).is_err());
                if !pointer {
                    assert!(session.comment_data(&file).is_err());
                }
            }
        }
    }

    #[test]
    fn manifest_round_trip_maps_only_exact_rhs_filenames() {
        let f = Fixture::new();
        let session = f.session();
        let file = session.head_root().join("src/example.rs");
        let loaded = load_for_file(&f.root, &f.state, &file).unwrap().unwrap();
        assert!(loaded.pr.same_revision(&pr()));
        assert_eq!(loaded.selection_path(&file).unwrap(), "src/example.rs");
        assert_eq!(loaded.merge_base, "c".repeat(40));
        assert_eq!(
            loaded.manifest.files[0].head.as_ref().unwrap().sha256,
            digest(b"new\n")
        );
        assert_eq!(
            repo_path(&loaded.head_root(), &file).unwrap(),
            "src/example.rs"
        );
        let ordinary = f.root.join("example.rs");
        fs::write(&ordinary, b"new\n").unwrap();
        assert!(
            load_for_file(&f.root, &f.state, &ordinary)
                .unwrap()
                .is_none()
        );
        assert!(loaded.selection_path(&ordinary).is_err());
    }

    #[test]
    fn reject_base_prompt_unknown_and_missing_state_files() {
        let f = Fixture::new();
        let session = f.session();
        let prompt = f.store.directory.join("prompt.txt");
        fs::write(&prompt, b"new\n").unwrap();
        for path in [
            session.directory.join("base/src/example.rs"),
            session.directory.join("manifest.json"),
            session.head_root(),
            session.head_root().join("unregistered.rs"),
            f.store.directory.join("unknown.txt"),
            prompt,
        ] {
            assert!(
                load_for_file(&f.root, &f.state, &path).is_err(),
                "{}",
                path.display()
            );
        }
        fs::write(session.head_root().join("unregistered.rs"), b"new\n").unwrap();
        assert!(
            session
                .selection_path(&session.head_root().join("unregistered.rs"))
                .is_err()
        );
        assert!(
            session
                .selection_path(&session.head_root().join("src/example.rs"))
                .is_err()
        );
    }

    #[test]
    fn active_pointer_does_not_rebind_old_tabs_or_hide_stale_revisions() {
        let f = Fixture::new();
        assert!(load_active(&f.root, &f.state).unwrap().is_none());
        let first = f.session();
        first.activate(&f.state).unwrap();
        let mut next = pr();
        next.head_sha = "d".repeat(40);
        let second = f.with_files(&next, &[changed("src/example.rs", "modified", None)]);
        assert_ne!(first.directory, second.directory);
        second.activate(&f.state).unwrap();
        assert_eq!(
            load_active(&f.root, &f.state).unwrap().unwrap().directory,
            second.directory
        );
        let old_tab = load_for_file(&f.root, &f.state, &first.head_root().join("src/example.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(old_tab.directory, first.directory);
        assert!(!old_tab.pr.same_revision(&next));
        for field in ["base", "reviewer", "head_repo", "head_ref"] {
            let mut current = pr();
            match field {
                "base" => current.base_sha = "e".repeat(40),
                "reviewer" => current.reviewer = "another".into(),
                "head_repo" => current.head_repo = "other/fork".into(),
                _ => current.head_ref = "another-topic".into(),
            }
            assert!(!old_tab.pr.same_revision(&current));
        }
    }

    #[test]
    fn altered_base_or_head_content_and_extra_directories_fail_closed() {
        for side in ["base", "head"] {
            let f = Fixture::new();
            let session = f.session();
            session.activate(&f.state).unwrap();
            replace(
                &session.directory.join(side).join("src/example.rs"),
                b"edited\n",
            );
            assert!(load_active(&f.root, &f.state).is_err());
            assert!(
                load_for_file(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .is_err()
            );
            assert!(session.activate(&f.state).is_err());
        }
        let f = Fixture::new();
        let session = f.session();
        fs::create_dir(session.head_root().join("unregistered")).unwrap();
        assert!(session.validate().is_err());
    }

    #[test]
    fn reject_manifest_tampering_and_wrong_root_identity_layout_or_version() {
        for field in [
            "root",
            "directory",
            "reviewer",
            "repo",
            "number",
            "version",
            "mapping",
            "snapshot",
            "digest",
            "duplicate",
            "membership",
            "traversal",
        ] {
            let f = Fixture::new();
            let session = f.session();
            change_manifest(&session, |manifest| match field {
                "root" => manifest.root = f.root.join("other"),
                "directory" => manifest.directory = f.store.directory.join("diff-other"),
                "reviewer" => manifest.pr.reviewer = "other".into(),
                "repo" => manifest.pr.repo = "other/repo".into(),
                "number" => manifest.pr.number += 1,
                "version" => manifest.version += 1,
                "mapping" => {
                    manifest.files[0].head.as_mut().unwrap().repository_path = "elsewhere.rs".into()
                }
                "snapshot" => {
                    manifest.files[0].head.as_mut().unwrap().path = "base/src/example.rs".into()
                }
                "digest" => manifest.files[0].head.as_mut().unwrap().sha256 = "0".repeat(64),
                "duplicate" => manifest.files.push(manifest.files[0].clone()),
                "membership" => manifest.files[0].head = None,
                _ => manifest.files[0].filename = "../example.rs".into(),
            });
            assert!(
                load_for_file(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .is_err(),
                "{field}"
            );
        }
        let f = Fixture::new();
        let session = f.session();
        session.activate(&f.state).unwrap();
        change_manifest(&session, |manifest| {
            manifest.pr.title = "different metadata".into()
        });
        assert!(session.validate().is_err());
        assert!(load_active(&f.root, &f.state).is_err());
    }

    #[test]
    fn additions_deletions_renames_copies_binary_and_empty_diffs() {
        let f = Fixture::new();
        let mut files = vec![
            changed("added.rs", "added", None),
            changed("removed.rs", "removed", None),
            changed("new/name.rs", "renamed", Some("old/name.rs")),
            changed("copy.rs", "copied", Some("source.rs")),
            changed("image.bin", "modified", None),
        ];
        files[2].patch = None; // A pure rename still has metadata and two snapshots.
        files[4].patch = None;
        let session = create_session(
            &f.root,
            &f.store,
            &pr(),
            &"c".repeat(40),
            &files,
            |_, path| {
                Ok(if path == "image.bin" {
                    b"\0binary".to_vec()
                } else {
                    b"text\n".to_vec()
                })
            },
        )
        .unwrap();
        for (base, head) in [("removed.rs", "added.rs"), ("old/name.rs", "new/name.rs")] {
            assert!(session.directory.join("base").join(base).is_file());
            assert!(session.head_root().join(head).is_file());
        }
        assert!(!session.directory.join("base/added.rs").exists());
        assert!(!session.directory.join("base/copy.rs").exists());
        assert!(!session.directory.join("base/source.rs").exists());
        assert!(!session.head_root().join("removed.rs").exists());
        assert_eq!(
            session
                .selection_path(&session.head_root().join("new/name.rs"))
                .unwrap(),
            "new/name.rs"
        );
        assert_eq!(
            fs::read(session.head_root().join("image.bin")).unwrap(),
            b"\0binary"
        );
        assert!(session.manifest.files[4].binary);
        let empty = f.with_files(&pr(), &[]);
        empty.activate(&f.state).unwrap();
        // The empty case does not invoke Zed at all.
        empty.open().unwrap();
        assert!(
            load_active(&f.root, &f.state)
                .unwrap()
                .unwrap()
                .manifest
                .files
                .is_empty()
        );
    }

    #[test]
    fn unsafe_paths_and_incorrect_store_are_rejected_before_writing_snapshots() {
        let f = Fixture::new();
        for filename in [
            "",
            "/absolute",
            "../escape",
            "a/../b",
            "a/./b",
            "a//b",
            "a\\b",
            "a\nb",
            "C:/drive",
        ] {
            let files = [changed(filename, "added", None)];
            assert!(
                create_session(&f.root, &f.store, &pr(), &"c".repeat(40), &files, |_, _| {
                    panic!("unsafe filename must not reach blob lookup")
                })
                .is_err(),
                "{filename:?}"
            );
        }
        let mut other = pr();
        other.reviewer = "another".into();
        assert!(
            create_session(
                &f.root,
                &f.store,
                &other,
                &"c".repeat(40),
                &[],
                |_, _| unreachable!()
            )
            .is_err()
        );
        let external = tempfile::tempdir().unwrap();
        assert!(load_active(&f.root, external.path()).is_err());
        assert!(load_active(external.path(), &f.state).is_err());
    }

    #[test]
    fn mapped_comments_keep_the_existing_blob_and_patch_checks() {
        use crate::selection::{Selection, make_comment};

        let f = Fixture::new();
        let session = f.session();
        let selection = Selection {
            file: session.head_root().join("src/example.rs"),
            row: 1,
            column: 1,
            text: "new".into(),
        };
        let (mapped, blob, files) = session.comment_data(&selection.file).unwrap();
        let mut files = files.unwrap();
        assert_eq!(mapped, session.selection_path(&selection.file).unwrap());
        let comment = make_comment(
            &session.head_root(),
            &selection,
            &blob,
            &files,
            "Feedback".into(),
        )
        .unwrap();
        assert_eq!(mapped, comment.path);
        assert_eq!(comment.path, "src/example.rs");
        assert!(
            make_comment(
                &session.head_root(),
                &selection,
                b"different pinned blob\n",
                &files,
                "Feedback".into(),
            )
            .is_err()
        );
        files[0].patch = None;
        assert!(
            make_comment(
                &session.head_root(),
                &selection,
                b"new\n",
                &files,
                "Feedback".into(),
            )
            .is_err()
        );
        assert!(
            make_comment(
                &session.head_root(),
                &selection,
                b"new\0\n",
                &files,
                "Feedback".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn corrupt_active_pointers_never_fall_back_to_another_session() {
        let f = Fixture::new();
        let session = f.session();
        session.activate(&f.state).unwrap();
        let path = f.state.join("active.json");
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for field in [
            "version",
            "root",
            "directory",
            "alias",
            "manifest_sha256",
            "unknown",
        ] {
            let mut active = original.clone();
            match field {
                "version" => active["version"] = 99.into(),
                "root" => active["root"] = f.root.join("other").to_str().unwrap().into(),
                "directory" => {
                    active["directory"] = f
                        .store
                        .directory
                        .join("diff-missing")
                        .to_str()
                        .unwrap()
                        .into()
                }
                "alias" => {
                    active["directory"] = format!("{}/head/..", session.directory.display()).into()
                }
                "manifest_sha256" => active["manifest_sha256"] = "0".repeat(64).into(),
                _ => active["unknown"] = true.into(),
            }
            fs::write(&path, serde_json::to_vec(&active).unwrap()).unwrap();
            assert!(load_active(&f.root, &f.state).is_err(), "{field}");
            // The active pointer is irrelevant when resolving an existing tab.
            assert!(
                load_for_file(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .unwrap()
                .is_some()
            );
        }
        fs::write(&path, b"{broken JSON").unwrap();
        assert!(load_active(&f.root, &f.state).is_err());
        session.activate(&f.state).unwrap();
        fs::remove_file(session.directory.join("manifest.json")).unwrap();
        assert!(load_active(&f.root, &f.state).is_err());
    }

    #[test]
    fn session_directory_cannot_be_relocated_even_under_the_same_draft_key() {
        let f = Fixture::new();
        let session = f.session();
        let moved = f.store.directory.join("diff-relocated");
        fs::rename(&session.directory, &moved).unwrap();
        assert!(load_for_file(&f.root, &f.state, &moved.join("head/src/example.rs")).is_err());
    }

    #[test]
    fn missing_state_is_not_an_error_for_ordinary_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let state = root.join(STATE_PATH);
        assert!(load_active(&root, &state).unwrap().is_none());
        assert!(
            load_for_file(&root, &state, &root.join("ordinary.rs"))
                .unwrap()
                .is_none()
        );
        assert!(load_for_file(&root, &state, &state.join("unknown.rs")).is_err());
    }

    #[test]
    fn selected_paths_must_have_exact_canonical_spelling() {
        let f = Fixture::new();
        let session = f.session();
        for suffix in [
            "head/src/../src/example.rs",
            "head/src/./example.rs",
            "head/src//example.rs",
            "head/src/example.rs/",
        ] {
            let file = PathBuf::from(format!("{}/{suffix}", session.directory.display()));
            assert!(load_for_file(&f.root, &f.state, &file).is_err(), "{suffix}");
        }
        let other_directory = f.store.directory.join("not-a-session");
        fs::rename(&session.directory, &other_directory).unwrap();
        assert!(
            load_for_file(
                &f.root,
                &f.state,
                &other_directory.join("head/src/example.rs")
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlinks_hardlinks_and_external_aliases() {
        use std::os::unix::fs::symlink;
        for target in [
            "manifest.json",
            "base/src/example.rs",
            "head/src/example.rs",
            "head/src",
            "head",
        ] {
            let f = Fixture::new();
            let session = f.session();
            let original = session.directory.join(target);
            let moved = f.root.join("moved");
            fs::rename(&original, &moved).unwrap();
            symlink(&moved, &original).unwrap();
            assert!(
                load_for_file(
                    &f.root,
                    &f.state,
                    &session.head_root().join("src/example.rs")
                )
                .is_err(),
                "{target}"
            );
        }
        let f = Fixture::new();
        let session = f.session();
        let alias = f.root.join("alias.rs");
        symlink(session.head_root().join("src/example.rs"), &alias).unwrap();
        assert!(load_for_file(&f.root, &f.state, &alias).is_err());
        let hardlink = f.root.join("hardlink.rs");
        fs::hard_link(session.head_root().join("src/example.rs"), &hardlink).unwrap();
        assert!(session.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlinked_active_pointer_session_and_state_ancestors() {
        use std::os::unix::fs::symlink;
        for target in ["active", "session", "key", "state", "scratch"] {
            let f = Fixture::new();
            let session = f.session();
            session.activate(&f.state).unwrap();
            let original = match target {
                "active" => f.state.join("active.json"),
                "session" => session.directory.clone(),
                "key" => f.store.directory.clone(),
                "state" => f.state.clone(),
                _ => f.root.join("scratch"),
            };
            let moved = f.root.join("moved");
            fs::rename(&original, &moved).unwrap();
            symlink(&moved, &original).unwrap();
            assert!(load_active(&f.root, &f.state).is_err(), "{target}");
            assert!(session.activate(&f.state).is_err(), "{target}");
        }
    }
}
