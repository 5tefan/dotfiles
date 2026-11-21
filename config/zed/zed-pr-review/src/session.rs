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

#[derive(Debug)]
pub struct Session {
    pub pr: PullRequest,
    pub root: PathBuf,
    pub directory: PathBuf,
    pub merge_base: String,
    manifest: Manifest,
    manifest_sha256: String,
    cleanup: Option<Cleanup>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    root: PathBuf,
    directory: PathBuf,
    pr: PullRequest,
    merge_base: String,
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    filename: String,
    status: String,
    previous_filename: Option<String>,
    has_patch: bool,
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

impl Session {
    /// Read pinned local blobs without fetching, checking out, or requiring jj @
    /// to match. The caller must check the current PR and jj @ before commenting.
    pub fn prepare(client: &Client, store: &Store, pr: &PullRequest) -> Result<Self> {
        let root = workspace_root(&client.root)?;
        let merge_base = client.merge_base(pr)?;
        let files = client.files(pr.number)?;
        let session = create_session(&root, store, pr, &merge_base, &files, |sha, path| {
            client.blob(sha, path)
        })?;
        ensure!(
            pr.same_revision(&client.read_pr(pr.number)?),
            "PR changed while preparing the full diff; prepare a new session"
        );
        Ok(session)
    }

    pub fn head_root(&self) -> PathBuf {
        self.directory.join("head")
    }

    /// Revalidate both the manifest and the complete snapshot before returning
    /// a repository filename. Never resolve an external file by its contents.
    pub fn selection_path(&self, file: &Path) -> Result<String> {
        self.validate()?;
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
        Ok(snapshot.repository_path.clone())
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
        ensure!(
            self.pr == self.manifest.pr
                && exact_path(&self.root, &self.manifest.root)
                && exact_path(&self.directory, &self.manifest.directory)
                && self.merge_base == self.manifest.merge_base,
            "Session identity was altered"
        );
        let current = load_directory(&self.root, &self.root.join(STATE_PATH), &self.directory)?;
        ensure!(
            current.manifest_sha256 == self.manifest_sha256,
            "Session manifest changed; prepare a new diff instead of reusing these snapshots"
        );
        ensure!(
            current.cleanup.is_none(),
            "Review snapshots were cleaned after submission; reopen the PR to prepare a new diff"
        );
        Ok(())
    }
}

/// Return None only for a file outside the review state. Files in review state
/// must belong to exactly one immutable session and be registered on its RHS.
pub fn load_for_file(root: &Path, state_base: &Path, file: &Path) -> Result<Option<Session>> {
    load_for_snapshot(root, state_base, file, false)
}

/// Load immutable context from either side, including removed snapshots after
/// cleanup. Also suitable for non-comment commands; comment coordinates remain
/// RHS-only. The caller must still check jj @ and the current PR revision.
pub fn load_for_navigation(root: &Path, state_base: &Path, file: &Path) -> Result<Option<Session>> {
    load_for_snapshot(root, state_base, file, true)
}

fn load_for_snapshot(
    root: &Path,
    state_base: &Path,
    file: &Path,
    allow_base: bool,
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
    if !allow_base {
        session.selection_path(file)?;
    } else if session.cleanup.is_none() {
        check_path(&root, file, false)?;
    }
    Ok(Some(session))
}

pub fn load_active(root: &Path, state_base: &Path) -> Result<Option<Session>> {
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
        let mut changed = session.cleanup.is_none();
        if changed {
            write_cleanup(&session, store, receipt)?;
        }
        let mut directories = BTreeSet::from([PathBuf::from("base"), PathBuf::from("head")]);
        for snapshot in session
            .manifest
            .files
            .iter()
            .flat_map(|file| file.base.iter().chain(file.head.iter()))
        {
            let path = session.directory.join(&snapshot.path);
            if exists_without_following(&path)? {
                check_path(&root, &path, false)?;
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
                check_path(&root, &path, true)?;
                fs::remove_dir(&path).with_context(|| {
                    format!("Cannot remove snapshot directory {}", path.display())
                })?;
                #[cfg(unix)]
                File::open(path.parent().context("Snapshot directory has no parent")?)?
                    .sync_all()?;
                changed = true;
            }
        }
        cleaned += usize::from(changed);
    }
    Ok(cleaned)
}

fn check_receipt(id: u64, state: &str) -> Result<()> {
    // Recovery verifies the original accepted review even if GitHub has since
    // dismissed it. Cleanup still requires the matching receipt on disk.
    ensure!(
        id > 0 && matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"),
        "Snapshot cleanup requires a valid approval, request-changes, or recovered dismissed receipt"
    );
    Ok(())
}

fn write_cleanup(session: &Session, store: &Store, receipt: &Receipt) -> Result<()> {
    let path = session.directory.join(CLEANUP_FILE);
    ensure!(
        !exists_without_following(&path)?,
        "Snapshot cleanup marker already exists; reload the session before cleanup"
    );
    let cleanup = Cleanup {
        version: VERSION,
        manifest_sha256: session.manifest_sha256.clone(),
        review_id: receipt.id,
        review_state: receipt.state.clone(),
        review_url: receipt.url.clone(),
    };
    // A crash before publication must not leave an unregistered temporary file
    // inside the immutable session and prevent a later cleanup attempt.
    let mut temporary = tempfile::NamedTempFile::new_in(&store.directory)?;
    serde_json::to_writer_pretty(&mut temporary, &cleanup)?;
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
    let temporary = tempfile::Builder::new()
        .prefix("diff-")
        .tempdir_in(&store.directory)?;
    let directory = temporary.path().to_path_buf();
    fs::create_dir(directory.join("base"))?;
    fs::create_dir(directory.join("head"))?;
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
            write_snapshot(root, &directory, &relative, &bytes)?;
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
        files: entries,
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    write_snapshot(root, &directory, "manifest.json", &bytes)?;
    let session = load_directory(root, &state_base, &directory)?;
    // Publish only complete sessions. A later PR recheck can fail, but such a
    // session stays inactive and cannot replace the session in an existing tab.
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
    check_path(root, directory, true)?;
    let path = directory.join("manifest.json");
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
    let cleanup_path = directory.join(CLEANUP_FILE);
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
    let mut actual_files = BTreeSet::new();
    let mut actual_dirs = BTreeSet::new();
    inventory(
        root,
        directory,
        directory,
        &mut actual_files,
        &mut actual_dirs,
    )?;
    let mut expected_files = BTreeSet::from(["manifest.json".to_owned()]);
    if cleanup.is_some() {
        expected_files.insert(CLEANUP_FILE.to_owned());
    }
    let mut expected_dirs = BTreeSet::from(["base".to_owned(), "head".to_owned()]);
    let mut filenames = BTreeSet::new();
    for file in &manifest.files {
        ensure!(
            filenames.insert(&file.filename),
            "Duplicate manifest filename"
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
                if cleanup.is_some() && !actual_files.contains(&snapshot.path) {
                    complete = false;
                    continue;
                }
                let path = directory.join(&snapshot.path);
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
    let valid_inventory = if cleanup.is_some() {
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

    #[test]
    fn cleanup_approval_and_request_changes_keep_comments_receipts_and_metadata() {
        for state in ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"] {
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
            "commented",
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
                "commented" => draft.submitted.as_mut().unwrap().state = "COMMENTED".into(),
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
        let mut files = [changed("src/example.rs", "modified", None)];
        let mapped = session.selection_path(&selection.file).unwrap();
        let comment = make_comment(
            &session.head_root(),
            &selection,
            b"new\n",
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
