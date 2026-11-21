use crate::model::{Comment, PullRequest};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const LOCAL_DIRECTORY: &str = "scratch/zed-pr-review";
const LOCAL_IGNORE: &[u8] = b"*\n";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attempt {
    pub payload: Value,
    pub previous_review_ids: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub id: u64,
    pub url: String,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Draft {
    pub version: u32,
    pub pr: PullRequest,
    pub comments: Vec<Comment>,
    pub attempt: Option<Attempt>,
    #[serde(default)]
    pub retry_history: Vec<Attempt>,
    pub submitted: Option<Receipt>,
}

impl Draft {
    pub fn new(pr: PullRequest) -> Self {
        Self {
            version: 1,
            pr,
            comments: vec![],
            attempt: None,
            retry_history: vec![],
            submitted: None,
        }
    }

    pub fn editable(&self) -> Result<()> {
        ensure!(
            self.attempt.is_none(),
            "A submission may already have reached GitHub. Run recover before editing, discarding, or submitting again"
        );
        ensure!(
            self.submitted.is_none(),
            "This draft has already been submitted. Use discard to explicitly start another review"
        );
        Ok(())
    }

    pub fn check_revision(&self, pr: &PullRequest) -> Result<()> {
        ensure!(
            self.pr.same_revision(pr),
            "The PR head, base, or identity has changed. Existing comments were kept. Inspect them with list, then discard explicitly and review the new revision"
        );
        Ok(())
    }
}

pub struct Store {
    pub directory: PathBuf,
    // Keep the file open for the entire operation, including terminal/editor input.
    // OS locks are released even when Ctrl-C or a crash skips Rust destructors.
    _lock: File,
}

impl Store {
    pub fn open(base: &Path, pr: &PullRequest) -> Result<Self> {
        private_directory(base)?;
        Self::at(&base.join(draft_key(pr)))
    }

    pub fn at(directory: &Path) -> Result<Self> {
        private_directory(directory)?;
        let directory = directory.canonicalize()?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock_path = directory.join("lock");
        regular_file_or_missing(&lock_path)?;
        let lock = options.open(lock_path)?;
        lock.try_lock().map_err(|error| {
            anyhow::anyhow!(
                "Another review command is using {}: {error}",
                directory.display()
            )
        })?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    pub fn load(&self) -> Result<Option<Draft>> {
        let path = self.directory.join("draft.json");
        regular_file_or_missing(&path)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("Cannot read draft"),
        };
        let draft: Draft = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "Invalid draft at {}; no data was overwritten",
                path.display()
            )
        })?;
        ensure!(
            draft.version == 1,
            "Unsupported draft version {}",
            draft.version
        );
        Ok(Some(draft))
    }

    pub fn save(&self, draft: &Draft) -> Result<()> {
        self.write_json("draft.json", draft)
    }

    pub fn write_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        ensure!(
            !name.is_empty()
                && !name.contains('/')
                && !name.contains('\\')
                && name != "."
                && name != "..",
            "Invalid state filename"
        );
        ensure!(
            name != "lock" && name != ".gitignore",
            "Reserved state filename"
        );
        regular_file_or_missing(&self.directory.join(name))?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join(name))
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        regular_file_or_missing(&self.directory.join("draft.json"))?;
        match fs::remove_file(self.directory.join("draft.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("Cannot clear archived draft"),
        }
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub fn archive(&self, draft: &Draft) -> Result<PathBuf> {
        let mut file = tempfile::Builder::new()
            .prefix("archive-")
            .suffix(".json")
            .tempfile_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut file, draft)?;
        file.as_file().sync_all()?;
        let (_, path) = file.keep()?;
        Ok(path)
    }
}

pub fn draft_key(pr: &PullRequest) -> String {
    let identity = format!(
        "{}\n{}\n{}\n{}",
        pr.host.to_ascii_lowercase(),
        pr.repo.to_ascii_lowercase(),
        pr.number,
        pr.reviewer.to_ascii_lowercase()
    );
    format!("{:x}", Sha256::digest(identity))
}

/// Select a location without creating it or migrating older external drafts.
pub fn default_directory(root: &Path) -> Result<PathBuf> {
    directory_override(root, std::env::var_os("ZED_PR_REVIEW_STATE_DIR"))
}

fn directory_override(root: &Path, path: Option<std::ffi::OsString>) -> Result<PathBuf> {
    match path {
        Some(path) => {
            ensure!(!path.is_empty(), "ZED_PR_REVIEW_STATE_DIR is empty");
            Ok(path.into())
        }
        None => Ok(root.join(LOCAL_DIRECTORY)),
    }
}

/// Prepare a canonical base for drafts and diff sessions before any private writes.
///
/// Only the exact local directory is allowed inside this checkout. Its untracked
/// `.gitignore` ignores itself as well as its contents, in both Git and jj. No
/// tracked files are changed or untracked. `incoming_head`, when present, must be
/// a full Git commit ID, checked without replacement refs or lazy fetching.
///
/// `Store::open`, `Store::at`, and `private_directory` do not establish repository
/// ignore protection themselves; callers must prepare the base first. An external
/// base is checked for unsafe files but does not require Git/jj or an incoming ID.
///
/// Call again before using state after a checkout change. Like Store's filesystem
/// API, this assumes no concurrent checkout, ignore edits, or hostile path swaps;
/// the per-PR lock serializes review commands, not arbitrary filesystem writers.
pub fn prepare_directory(
    root: &Path,
    git_dir: &Path,
    path: &Path,
    incoming_head: Option<&str>,
) -> Result<PathBuf> {
    let directory = safe_base(root, path)?;
    check_existing_tree(&directory)?;
    let root = root
        .canonicalize()
        .context("Cannot resolve repository root")?;
    if directory == root.join(LOCAL_DIRECTORY) {
        ensure_untracked(&root, git_dir, incoming_head)?;
        private_directory(&directory)?;
        install_ignore(&directory)?;
        verify_git_ignore(&root, git_dir)?;
    } else {
        private_directory(&directory)?;
    }
    Ok(directory)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "State path is empty");
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "Parent traversal is not allowed in state paths: {}",
        path.display()
    );
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(path.components().collect())
}

// Resolve existing ancestors without creating anything, including dangling links.
fn resolved_directory(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() || metadata.file_type().is_symlink(),
                "State path is not a directory: {}",
                path.display()
            );
            path.canonicalize()
                .with_context(|| format!("Cannot resolve {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("State path has no parent")?;
            Ok(resolved_directory(parent)?
                .join(path.file_name().context("Missing state basename")?))
        }
        Err(error) => Err(error).with_context(|| format!("Cannot inspect {}", path.display())),
    }
}

fn safe_base(root: &Path, path: &Path) -> Result<PathBuf> {
    let root_path = absolute_path(root)?;
    let root = root_path
        .canonicalize()
        .context("Cannot resolve repository root")?;
    let path = absolute_path(path)?;
    let resolved = resolved_directory(&path)?;
    // Permit the spelling supplied for the root (e.g. macOS /var -> /private/var),
    // but not an independent symlink that enters the repository from elsewhere.
    if let Ok(relative) = path
        .strip_prefix(&root_path)
        .or_else(|_| path.strip_prefix(&root))
    {
        ensure!(
            relative == Path::new(LOCAL_DIRECTORY),
            "Repository-local state is only allowed at {}",
            root.join(LOCAL_DIRECTORY).display()
        );
        check_directory_components(&root, relative)?;
        ensure!(
            resolved == root.join(LOCAL_DIRECTORY),
            "State path resolves outside its expected location"
        );
    } else {
        ensure!(
            !resolved.starts_with(&root),
            "A symlink alias into the repository is not a safe state directory"
        );
        ensure!(
            !root.starts_with(&resolved),
            "State directory must not contain the repository"
        );
        // Existing external ancestors may use system aliases, but the state
        // directory itself must not be a symlink.
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "State directory must not be a symlink: {}",
                path.display()
            );
        }
        // Also reject paths that pass through the checkout before leaving it.
        for ancestor in path.ancestors().skip(1) {
            ensure!(
                !resolved_directory(ancestor)?.starts_with(&root),
                "State path must not pass through a repository symlink"
            );
        }
    }
    Ok(resolved)
}

fn check_directory_components(base: &Path, relative: &Path) -> Result<()> {
    let mut current = base.to_owned();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "State path contains a symlink or non-directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

fn regular_file_or_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "State file must be a regular file, not a symlink: {}",
                path.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.nlink() == 1,
                    "State file must not have hard links: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Cannot inspect {}", path.display())),
    }
}

fn check_existing_tree(directory: &Path) -> Result<()> {
    check_tree_entries(directory, true)
}

// Every retained snapshot is still scanned for unsafe files. Only the base's
// .gitignore configures state storage; nested ignore files may be PR source.
fn check_tree_entries(directory: &Path, is_base: bool) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "State directory must not be a symlink: {}",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("Cannot inspect existing state"),
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if is_base
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(".gitignore")
        {
            regular_file_or_missing(&entry.path())?;
            ensure!(
                fs::read(entry.path())? == LOCAL_IGNORE,
                "Conflicting state ignore file at {}; no files were changed",
                entry.path().display()
            );
        } else if entry.file_type()?.is_dir() {
            check_tree_entries(&entry.path(), false)?;
        } else {
            regular_file_or_missing(&entry.path())?;
        }
    }
    Ok(())
}

fn local_command(program: &str, root: &Path) -> Command {
    let mut command = Command::new(program);
    // An inherited Git index, object directory, or config override must not make
    // these checks inspect a different repository or a different ignore policy.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_")
            && !matches!(
                name.to_str(),
                Some("GIT_CONFIG_NOSYSTEM" | "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM")
            )
        {
            command.env_remove(name);
        }
    }
    command.current_dir(root).stdin(Stdio::null());
    command
}

fn git_command(root: &Path, git_dir: &Path) -> Command {
    let mut command = local_command("git", root);
    command
        .args([
            "--no-pager",
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "--git-dir",
        ])
        .arg(git_dir)
        .arg("--work-tree")
        .arg(root)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn output(command: &mut Command, action: &str) -> Result<Vec<u8>> {
    let result = command
        .output()
        .with_context(|| format!("Cannot {action}"))?;
    ensure!(
        result.status.success(),
        "Cannot {action}: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(result.stdout)
}

fn state_entry(path: &[u8]) -> bool {
    // Reject case aliases even on a case-sensitive host: an incoming tree may
    // later be checked out on macOS and overwrite the same directory.
    let mut components = path.split(|byte| *byte == b'/');
    components
        .next()
        .is_some_and(|part| part.eq_ignore_ascii_case(b"scratch"))
        && components
            .next()
            .is_none_or(|part| part.eq_ignore_ascii_case(b"zed-pr-review"))
}

fn ensure_untracked(root: &Path, git_dir: &Path, incoming_head: Option<&str>) -> Result<()> {
    let mut jj = local_command("jj", root);
    let tracked = output(
        jj.env("JJ_PAGER", "cat")
            .args([
                "--ignore-working-copy",
                "--no-pager",
                "--color=never",
                "--repository",
            ])
            .arg(root)
            .args(["file", "list", "-r", "@", "-T", "path ++ \"\\0\""]),
        "check tracked state paths in jj @",
    )?;
    ensure!(
        !tracked.split(|byte| *byte == 0).any(state_entry),
        "Private state paths are tracked in jj @; move state outside the repository. Nothing was untracked"
    );
    let index = output(
        git_command(root, git_dir).args(["ls-files", "--cached", "--full-name", "-z"]),
        "check the Git index for state paths",
    )?;
    ensure!(
        !index.split(|byte| *byte == 0).any(state_entry),
        "Private state paths are tracked in the Git index; nothing was untracked"
    );
    if let Some(head) = incoming_head {
        ensure!(
            (head.len() == 40 || head.len() == 64)
                && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Incoming head must be a full Git commit ID"
        );
        let kind = output(
            git_command(root, git_dir).args(["cat-file", "-t", head]),
            "check incoming commit type",
        )?;
        ensure!(kind == b"commit\n", "Incoming head is not a Git commit");
        let tree = output(
            git_command(root, git_dir).args([
                "ls-tree",
                "-r",
                "--full-tree",
                "--name-only",
                "-z",
                head,
            ]),
            "check incoming head for tracked state paths",
        )?;
        ensure!(
            !tree.split(|byte| *byte == 0).any(state_entry),
            "Private state paths are tracked in the incoming head; move state outside the repository. Nothing was untracked"
        );
    }
    Ok(())
}

fn install_ignore(directory: &Path) -> Result<()> {
    let path = directory.join(".gitignore");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(LOCAL_IGNORE)?;
            file.sync_all()?;
            #[cfg(unix)]
            File::open(directory)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            regular_file_or_missing(&path)?;
            ensure!(
                fs::read(&path)? == LOCAL_IGNORE,
                "Conflicting state ignore file at {}; no files were overwritten",
                path.display()
            );
        }
        Err(error) => return Err(error).context("Cannot install the local state ignore file"),
    }
    Ok(())
}

fn verify_git_ignore(root: &Path, git_dir: &Path) -> Result<()> {
    // Check files without creating probe data. The local '*' rule also prevents
    // Git and jj from descending into future draft/session directories.
    for path in [
        "scratch/zed-pr-review/.gitignore",
        "scratch/zed-pr-review/draft.json",
        "scratch/zed-pr-review/sessions/probe.json",
    ] {
        output(
            git_command(root, git_dir).args(["check-ignore", "--no-index", "-q", "--", path]),
            "verify that local review state is ignored by Git",
        )?;
    }
    Ok(())
}

pub fn private_directory(path: &Path) -> Result<()> {
    let path = absolute_path(path)?;
    create_private_directories(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

// Do not change permissions on existing parents (in particular, the checkout).
fn create_private_directories(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "State directory must not be a symlink: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("State directory has no parent")?;
            create_private_directories(parent)?;
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        fs::symlink_metadata(path)?.is_dir(),
                        "State directory was replaced by a symlink"
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("Cannot create {}", path.display()));
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot inspect {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PullRequest {
        PullRequest {
            host: "github.com".into(),
            repo: "a/b".into(),
            number: 42,
            url: "https://github.com/a/b/pull/42".into(),
            title: "Review".into(),
            head_sha: "a".repeat(40),
            base_sha: "b".repeat(40),
            head_repo: "a/b".into(),
            head_ref: "topic".into(),
            author: "author".into(),
            reviewer: "reviewer".into(),
        }
    }

    #[test]
    fn default_and_override_are_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        assert_eq!(
            directory_override(&root, None).unwrap(),
            root.join(LOCAL_DIRECTORY)
        );
        let old = temp
            .path()
            .join("Library/Application Support/zed-pr-review");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("old-draft"), "kept").unwrap();
        assert_eq!(
            directory_override(&root, Some(old.clone().into())).unwrap(),
            old
        );
        assert!(directory_override(&root, Some("".into())).is_err());
        assert!(!root.exists());
        assert_eq!(fs::read(old.join("old-draft")).unwrap(), b"kept");
    }

    #[test]
    fn tracked_path_checks_include_case_aliases_and_ancestors() {
        for path in [
            "scratch",
            "SCRATCH/zed-pr-review",
            "scratch/ZED-PR-REVIEW/.gitignore",
            "scratch/zed-pr-review/drafts/file\nname.json",
        ] {
            assert!(state_entry(path.as_bytes()), "missed {path}");
        }
        for path in [
            "",
            "scratch-other",
            "scratch/other",
            "scratch/zed-pr-review-old/file",
            "elsewhere/scratch/zed-pr-review",
        ] {
            assert!(!state_entry(path.as_bytes()), "rejected {path}");
        }
    }

    #[test]
    fn paths_are_checked_before_creation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("repo");
        fs::create_dir(&root).unwrap();
        assert_eq!(
            safe_base(&root, &root.join(LOCAL_DIRECTORY)).unwrap(),
            root.join(LOCAL_DIRECTORY)
        );
        let external = root.with_file_name("external").join("new");
        assert_eq!(safe_base(&root, &external).unwrap(), external);
        for path in [
            root.clone(),
            root.join("scratch"),
            root.join("private"),
            root.join("scratch/zed-pr-review/nested"),
            root.join("scratch/../scratch/zed-pr-review"),
            root.join("../external"),
            root.parent().unwrap().to_owned(),
        ] {
            assert!(
                safe_base(&root, &path).is_err(),
                "accepted {}",
                path.display()
            );
        }
        assert!(!root.join("scratch").exists());
        assert!(!external.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_special_state_files_are_refused() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("repo");
        let external = base.join("external");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&external).unwrap();
        symlink(&root, base.join("alias")).unwrap();
        assert!(safe_base(&root, &base.join("alias/scratch/zed-pr-review")).is_err());
        symlink(&external, root.join("scratch")).unwrap();
        assert!(safe_base(&root, &root.join(LOCAL_DIRECTORY)).is_err());
        assert!(!external.join("zed-pr-review").exists());
        fs::remove_file(root.join("scratch")).unwrap();
        fs::create_dir(root.join("scratch")).unwrap();
        symlink(&external, root.join(LOCAL_DIRECTORY)).unwrap();
        assert!(safe_base(&root, &root.join(LOCAL_DIRECTORY)).is_err());
        fs::remove_file(root.join(LOCAL_DIRECTORY)).unwrap();
        symlink(base.join("missing"), root.join(LOCAL_DIRECTORY)).unwrap();
        assert!(safe_base(&root, &root.join(LOCAL_DIRECTORY)).is_err());
        assert!(private_directory(&root.join(LOCAL_DIRECTORY)).is_err());
        fs::remove_file(root.join(LOCAL_DIRECTORY)).unwrap();
        symlink(&external, base.join("external-alias")).unwrap();
        assert!(safe_base(&root, &base.join("external-alias")).is_err());

        let store = Store::open(&external, &pr()).unwrap();
        let victim = base.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, store.directory.join("draft.json")).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&Draft::new(pr())).is_err());
        assert!(store.clear().is_err());
        assert!(check_existing_tree(&external).is_err());
        fs::remove_file(store.directory.join("draft.json")).unwrap();
        fs::hard_link(&victim, store.directory.join("draft.json")).unwrap();
        assert!(store.load().is_err());
        fs::remove_file(store.directory.join("draft.json")).unwrap();
        for side in ["base", "head"] {
            let path = external
                .join("sessions/one")
                .join(side)
                .join("src/.gitignore");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink(&victim, &path).unwrap();
            assert!(check_existing_tree(&external).is_err());
            fs::remove_file(&path).unwrap();
            fs::hard_link(&victim, &path).unwrap();
            assert!(check_existing_tree(&external).is_err());
            fs::remove_file(&path).unwrap();
            fs::write(&path, "generated/\n!private.rs\n").unwrap();
            assert!(check_existing_tree(&external).is_ok());
        }
        assert!(store.write_json("lock", &Value::Null).is_err());
        assert!(store.write_json(".gitignore", &Value::Null).is_err());
        let directory = store.directory.clone();
        drop(store);
        fs::remove_file(directory.join("lock")).unwrap();
        symlink(&victim, directory.join("lock")).unwrap();
        assert!(Store::at(&directory).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
    }

    /// Re-execute only this test in a child with no inherited Git/JJ configuration
    /// or credentials. Production commands then inherit the same isolated config
    /// as the fixture commands, without changing the parallel test runner's env.
    #[test]
    fn isolated_repository_privacy() {
        const CHILD: &str = "ZED_PR_REVIEW_STORE_TEST_ROOT";
        if let Some(base) = std::env::var_os(CHILD) {
            let base = PathBuf::from(base);
            assert_eq!(std::env::current_dir().unwrap(), base);
            real_repository_checks(&base);
            return;
        }
        let temp = tempfile::Builder::new()
            .prefix("zed-review-store-")
            .tempdir()
            .unwrap();
        let base = temp.path().canonicalize().unwrap();
        fs::create_dir(base.join("home")).unwrap();
        fs::create_dir(base.join("tmp")).unwrap();
        fs::write(
            base.join("jj.toml"),
            "[user]\nname = 'Store Tests'\nemail = 'store@example.test'\n[ui]\npager = 'cat'\n",
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::isolated_repository_privacy",
                "--nocapture",
            ])
            .env_clear()
            .current_dir(&base)
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("home"))
            .env("TMPDIR", base.join("tmp"))
            .env("JJ_CONFIG", base.join("jj.toml"))
            .env("JJ_PAGER", "cat")
            .env("PAGER", "cat")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env(CHILD, &base)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "isolated storage checks failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    struct Repository {
        root: PathBuf,
        git: PathBuf,
    }

    impl Repository {
        fn new(base: &Path, name: &str, colocated: bool) -> Self {
            let root = base.join(name);
            fs::create_dir(&root).unwrap();
            let mut init = local_command("jj", &root);
            init.env("JJ_PAGER", "cat").args(["git", "init"]);
            init.arg(if colocated {
                "--colocate"
            } else {
                "--no-colocate"
            });
            output(&mut init, "initialize temporary jj repository").unwrap();
            let git = output(
                local_command("jj", &root).args(["--ignore-working-copy", "git", "root"]),
                "locate temporary Git directory",
            )
            .unwrap();
            let git = PathBuf::from(String::from_utf8(git).unwrap().trim());
            let repo = Self { root, git };
            repo.jj(&["describe", "-m", "Empty test commit"]);
            repo
        }

        fn jj(&self, args: &[&str]) -> Vec<u8> {
            output(
                local_command("jj", &self.root)
                    .env("JJ_PAGER", "cat")
                    .args(["--no-pager", "--color=never"])
                    .args(args),
                "run jj in temporary repository",
            )
            .unwrap()
        }

        fn git(&self, args: &[&str]) -> Vec<u8> {
            output(
                git_command(&self.root, &self.git).args(args),
                "run Git in temporary repository",
            )
            .unwrap()
        }

        fn head(&self) -> String {
            String::from_utf8(self.jj(&[
                "--ignore-working-copy",
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ]))
            .unwrap()
            .trim()
            .to_owned()
        }

        fn state(&self) -> PathBuf {
            self.root.join(LOCAL_DIRECTORY)
        }

        fn prepare(&self, incoming: Option<&str>) -> Result<PathBuf> {
            prepare_directory(&self.root, &self.git, &self.state(), incoming)
        }
    }

    fn real_repository_checks(base: &Path) {
        for colocated in [false, true] {
            let repo = Repository::new(
                base,
                if colocated { "colocated" } else { "separate" },
                colocated,
            );
            let ancestor_ignore = b"!scratch/**\n!scratch/zed-pr-review/**\n";
            fs::write(repo.root.join(".gitignore"), ancestor_ignore).unwrap();
            repo.jj(&["describe", "-m", "Tracked ancestor ignore file"]);
            let before = repo.head();
            #[cfg(unix)]
            let root_mode = {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(&repo.root).unwrap().permissions().mode()
            };
            assert_eq!(repo.prepare(Some(&before)).unwrap(), repo.state());
            assert_eq!(repo.prepare(None).unwrap(), repo.state());
            assert_eq!(
                fs::read(repo.state().join(".gitignore")).unwrap(),
                LOCAL_IGNORE
            );
            let store = Store::open(&repo.state(), &pr()).unwrap();
            store.save(&Draft::new(pr())).unwrap();
            store.archive(&Draft::new(pr())).unwrap();
            let session = Store::at(&repo.state().join("sessions/one")).unwrap();
            session
                .write_json(
                    "manifest.json",
                    &serde_json::json!({"key": draft_key(&pr())}),
                )
                .unwrap();
            fs::write(
                session.directory.join("private.diff"),
                "private diff contents\n",
            )
            .unwrap();
            for side in ["base", "head"] {
                let snapshot = session.directory.join(side);
                fs::create_dir_all(snapshot.join("src")).unwrap();
                fs::write(snapshot.join(".gitignore"), "target/\n.env\n!source.rs\n").unwrap();
                fs::write(snapshot.join("source.rs"), "// private source\n").unwrap();
                fs::write(snapshot.join("src/.gitignore"), "generated/\n!private.rs\n").unwrap();
                fs::write(
                    snapshot.join("src/private.rs"),
                    "// private nested source\n",
                )
                .unwrap();
            }
            // Later commands must accept source ignore files without letting
            // their negations expose the enclosing ignored snapshot tree.
            assert_eq!(repo.prepare(None).unwrap(), repo.state());
            assert_eq!(
                fs::read(repo.state().join(".gitignore")).unwrap(),
                LOCAL_IGNORE
            );
            for side in ["base", "head"] {
                let snapshot = session.directory.join(side);
                assert_eq!(
                    fs::read(snapshot.join(".gitignore")).unwrap(),
                    b"target/\n.env\n!source.rs\n"
                );
                assert_eq!(
                    fs::read(snapshot.join("src/.gitignore")).unwrap(),
                    b"generated/\n!private.rs\n"
                );
            }
            assert_eq!(repo.head(), before, "preparation must not snapshot jj");
            repo.jj(&["status"]); // A normal command must snapshot, not just ignore the working copy.
            assert_eq!(repo.head(), before, "private writes changed jj @");
            assert_eq!(
                fs::read(repo.root.join(".gitignore")).unwrap(),
                ancestor_ignore
            );
            assert!(repo.jj(&["file", "list", "root:scratch"]).is_empty());
            assert!(
                repo.git(&["ls-files", "--cached", "--", "scratch"])
                    .is_empty()
            );
            assert!(
                repo.git(&[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "--",
                    "scratch"
                ])
                .is_empty()
            );
            let ignored = String::from_utf8(repo.git(&[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--",
                "scratch",
            ]))
            .unwrap();
            for name in [".gitignore", "draft.json", "manifest.json", "private.diff"] {
                assert!(
                    ignored.contains(name),
                    "Git did not ignore {name}: {ignored}"
                );
            }
            for side in ["base", "head"] {
                for file in [
                    ".gitignore",
                    "source.rs",
                    "src/.gitignore",
                    "src/private.rs",
                ] {
                    let path = format!("scratch/zed-pr-review/sessions/one/{side}/{file}");
                    assert!(
                        ignored.lines().any(|line| line == path),
                        "Git did not ignore {path}"
                    );
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&repo.root).unwrap().permissions().mode(),
                    root_mode
                );
                assert_eq!(
                    fs::metadata(repo.state()).unwrap().permissions().mode() & 0o777,
                    0o700
                );
                assert_eq!(
                    fs::metadata(store.directory.join("draft.json"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }

        let repo = Repository::new(base, "tracked-current", false);
        fs::create_dir_all(repo.state()).unwrap();
        fs::write(repo.state().join(".gitignore"), "# initially tracked\n").unwrap();
        repo.jj(&["describe", "-m", "Tracked ignore file"]);
        let before = repo.head();
        fs::write(repo.state().join(".gitignore"), LOCAL_IGNORE).unwrap();
        let error = repo.prepare(None).unwrap_err().to_string();
        assert!(error.contains("tracked in jj @"), "{error}");
        assert_eq!(repo.head(), before);
        assert_eq!(
            fs::read(repo.state().join(".gitignore")).unwrap(),
            LOCAL_IGNORE
        );

        let repo = Repository::new(base, "incoming", false);
        fs::create_dir_all(repo.state()).unwrap();
        fs::write(repo.state().join("secret.json"), "tracked upstream").unwrap();
        repo.jj(&["describe", "-m", "Incoming tracked state"]);
        let incoming = repo.head();
        repo.jj(&["new", "root()", "-m", "Clean destination"]);
        let before = repo.head();
        // Replacement refs must not conceal the incoming tree's tracked path.
        repo.git(&["replace", &incoming, &before]);
        let error = repo.prepare(Some(&incoming)).unwrap_err().to_string();
        assert!(error.contains("tracked in the incoming head"), "{error}");
        assert!(!repo.state().exists());
        assert_eq!(repo.head(), before);
        for head in ["HEAD", "--help", "../bad", &"f".repeat(40)] {
            assert!(repo.prepare(Some(head)).is_err());
            assert!(!repo.state().exists());
        }

        for (name, path) in [
            ("incoming-ignore", "scratch/zed-pr-review/.gitignore"),
            ("incoming-case-alias", "Scratch/ZED-PR-REVIEW/secret.json"),
            ("incoming-ancestor-file", "scratch"),
        ] {
            let repo = Repository::new(base, name, false);
            let path = repo.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "# tracked upstream\n").unwrap();
            repo.jj(&["describe", "-m", "Incoming conflicting path"]);
            let incoming = repo.head();
            repo.jj(&["new", "root()", "-m", "Clean destination"]);
            let before = repo.head();
            let error = repo.prepare(Some(&incoming)).unwrap_err().to_string();
            assert!(
                error.contains("tracked in the incoming head"),
                "{name}: {error}"
            );
            assert!(!repo.state().exists());
            assert_eq!(repo.head(), before);
        }

        let repo = Repository::new(base, "index", true);
        fs::create_dir_all(repo.state()).unwrap();
        fs::write(repo.state().join("draft.json"), "staged only").unwrap();
        repo.git(&["add", "--", "scratch/zed-pr-review/draft.json"]);
        assert!(repo.prepare(None).is_err());
        assert!(!repo.state().join(".gitignore").exists());

        let repo = Repository::new(base, "conflicting", false);
        let before = repo.head();
        fs::create_dir_all(repo.state()).unwrap();
        let path = repo.state().join(".gitignore");
        fs::write(&path, "!private.json\n").unwrap();
        let error = repo.prepare(None).unwrap_err().to_string();
        assert!(error.contains("Conflicting"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"!private.json\n");
        fs::remove_file(path).unwrap();
        assert_eq!(repo.head(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let victim = base.join("ignore-victim");
            fs::write(&victim, LOCAL_IGNORE).unwrap();
            symlink(&victim, repo.state().join(".gitignore")).unwrap();
            assert!(repo.prepare(None).is_err());
            assert_eq!(fs::read(&victim).unwrap(), LOCAL_IGNORE);
            fs::remove_file(repo.state().join(".gitignore")).unwrap();
        }
        let outside = base.join("old external drafts");
        fs::create_dir(&outside).unwrap();
        let store = Store::open(&outside, &pr()).unwrap();
        store.save(&Draft::new(pr())).unwrap();
        drop(store);
        assert_eq!(
            prepare_directory(&repo.root, &repo.git, &outside, None).unwrap(),
            outside
        );
        assert!(
            Store::open(&outside, &pr())
                .unwrap()
                .load()
                .unwrap()
                .is_some()
        );
        assert!(!outside.join(".gitignore").exists());
    }

    #[test]
    fn locks_release_and_atomic_writes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), &pr()).unwrap();
        assert!(Store::open(dir.path(), &pr()).is_err());
        assert!(store.load().unwrap().is_none());
        let draft = Draft::new(pr());
        store.save(&draft).unwrap();
        assert!(store.load().unwrap().unwrap().pr.same_revision(&pr()));
        store.save(&draft).unwrap();
        drop(store);
        assert!(Store::open(dir.path(), &pr()).is_ok());
    }

    #[test]
    fn drafts_are_separate_by_repository_pr_and_account() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open(dir.path(), &pr()).unwrap();
        for field in ["repo", "reviewer", "number", "host"] {
            let mut other = pr();
            match field {
                "repo" => other.repo = "other/repo".into(),
                "reviewer" => other.reviewer = "other".into(),
                "host" => other.host = "github.example.com".into(),
                _ => other.number += 1,
            }
            assert_ne!(
                first.directory,
                Store::open(dir.path(), &other).unwrap().directory
            );
        }
    }

    #[test]
    fn unsafe_state_cannot_be_edited() {
        let mut draft = Draft::new(pr());
        let mut updated = pr();
        updated.base_sha = "c".repeat(40);
        assert!(draft.check_revision(&updated).is_err());
        draft.attempt = Some(Attempt {
            payload: Value::Null,
            previous_review_ids: vec![],
        });
        assert!(draft.editable().is_err());
        draft.attempt = None;
        draft.submitted = Some(Receipt {
            id: 1,
            url: String::new(),
            state: "APPROVED".into(),
        });
        assert!(draft.editable().is_err());
    }

    #[test]
    fn corrupt_or_future_drafts_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), &pr()).unwrap();
        fs::write(store.directory.join("draft.json"), "bad JSON").unwrap();
        assert!(store.load().is_err());
        let mut draft = Draft::new(pr());
        draft.version = 99;
        store.save(&draft).unwrap();
        assert!(store.load().is_err());
    }
}
