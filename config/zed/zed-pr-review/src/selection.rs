use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::model::{ChangedFile, Comment};

/// Zed 1.29.2 supplies the start of the newest selection, even for a reverse
/// selection. Rows and UTF-8 byte columns are one-based, not cursor endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub file: PathBuf,
    pub row: u32,
    pub column: u32,
    pub text: String,
}

impl Selection {
    pub fn from_env() -> Result<Self> {
        Self::from_values(
            std::env::var_os("ZED_FILE"),
            std::env::var_os("ZED_ROW"),
            std::env::var_os("ZED_COLUMN"),
            std::env::var_os("ZED_SELECTED_TEXT"),
        )
    }

    fn from_values(
        file: Option<OsString>,
        row: Option<OsString>,
        column: Option<OsString>,
        text: Option<OsString>,
    ) -> Result<Self> {
        let file = PathBuf::from(file.context("ZED_FILE is missing")?);
        ensure!(file.is_absolute(), "ZED_FILE must be an absolute file path");
        let row = env_string(row, "ZED_ROW")?;
        let column = env_string(column, "ZED_COLUMN")?;
        let row = decimal(&row).context("ZED_ROW must be a positive one-based row")?;
        let column =
            decimal(&column).context("ZED_COLUMN must be a positive one-based byte column")?;
        ensure!(
            row > 0 && column > 0,
            "ZED_ROW and ZED_COLUMN must be positive"
        );
        let text = env_string(text, "ZED_SELECTED_TEXT")?;
        ensure!(
            !text.trim().is_empty(),
            "select a non-whitespace excerpt in Zed"
        );
        Ok(Self {
            file,
            row,
            column,
            text,
        })
    }
}

fn env_string(value: Option<OsString>, name: &str) -> Result<String> {
    value
        .with_context(|| format!("{name} is missing"))?
        .into_string()
        .map_err(|_| anyhow!("{name} must be UTF-8"))
}

fn decimal(value: &str) -> Result<u32> {
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
        "expected an unsigned decimal integer"
    );
    value.parse().context("integer exceeds u32")
}

fn safe_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "repository-relative path is empty");
    ensure!(
        !path.contains('\\') && !path.chars().any(char::is_control),
        "repository-relative path contains a backslash or control character"
    );
    ensure!(
        !path.split('/').any(|part| matches!(part, "" | "." | "..")),
        "repository-relative path must not be absolute or contain empty, dot, or parent components"
    );
    let bytes = path.as_bytes();
    ensure!(
        !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'),
        "repository-relative path must not have a drive prefix"
    );
    Ok(())
}

/// Resolve a real file beneath the configured root without accepting a symlink
/// alias for any component below that root. The root and its ancestors may be
/// symlinks, and callers may use either the configured or canonical root path.
pub fn repo_path(root: &Path, file: &Path) -> Result<String> {
    ensure!(file.is_absolute(), "selected file must be absolute");
    let absolute_root = std::path::absolute(root).context("cannot resolve repository root")?;
    let canonical_root = fs::canonicalize(root).context("cannot canonicalize repository root")?;
    ensure!(
        canonical_root.is_dir(),
        "repository root is not a directory"
    );
    let canonical_file = fs::canonicalize(file).context("cannot canonicalize selected file")?;
    ensure!(
        canonical_file.starts_with(&canonical_root),
        "selected file is outside the repository"
    );
    let relative = file
        .strip_prefix(&absolute_root)
        .or_else(|_| file.strip_prefix(&canonical_root))
        .context("selected file uses an alias outside the configured repository root")?;
    let relative = relative
        .to_str()
        .context("repository-relative path must be UTF-8")?;
    safe_path(relative)?;

    let mut checked = canonical_root.clone();
    for component in relative.split('/') {
        checked.push(component);
        let metadata = fs::symlink_metadata(&checked)
            .with_context(|| format!("cannot inspect {}", checked.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symlink components below the repository root are not supported"
        );
    }
    ensure!(
        checked == canonical_file,
        "selected file resolves to a different repository path"
    );
    ensure!(
        canonical_file.is_file(),
        "selected path is not a regular file"
    );
    Ok(relative.to_owned())
}

fn normalize_line_endings(text: &str) -> Result<Cow<'_, str>> {
    if text.contains('\r') {
        let normalized = text.replace("\r\n", "\n");
        // Git counts LF-delimited lines; treating bare CR as a newline can misplace comments.
        ensure!(
            !normalized.contains('\r'),
            "bare CR characters are not supported; use LF or CRLF line endings so Git and Zed rows agree"
        );
        Ok(Cow::Owned(normalized))
    } else {
        Ok(Cow::Borrowed(text))
    }
}

fn text_blob(content: &[u8]) -> Result<&str> {
    ensure!(
        !content.contains(&0),
        "file contains NUL bytes; binary files are not supported"
    );
    std::str::from_utf8(content).context("file must contain UTF-8 text")
}

/// Return inclusive RHS rows for an exact, end-exclusive selection. Zed uses
/// normalized line endings; normalize CRLF in both inputs and reject bare CR,
/// but never trim the excerpt or search for a matching occurrence elsewhere.
pub fn line_range(content: &str, row: u32, column: u32, text: &str) -> Result<(u32, u32)> {
    ensure!(
        row > 0 && column > 0,
        "row and byte column must be one-based"
    );
    let content = normalize_line_endings(content)?;
    let text = normalize_line_endings(text)?;
    ensure!(
        !text.trim().is_empty(),
        "selection must contain non-whitespace text"
    );

    let mut lines = content.split('\n');
    let mut line_start = 0usize;
    for _ in 1..row {
        let line = lines.next().context("selection row is outside the file")?;
        line_start = line_start
            .checked_add(line.len())
            .and_then(|offset| offset.checked_add(1))
            .context("selection offset overflow")?;
    }
    let line = lines.next().context("selection row is outside the file")?;
    let byte_column = usize::try_from(column - 1).context("byte column exceeds usize")?;
    ensure!(
        byte_column <= line.len(),
        "selection byte column is outside its starting line"
    );
    ensure!(
        line.is_char_boundary(byte_column),
        "selection byte column splits a UTF-8 character"
    );
    let start = line_start
        .checked_add(byte_column)
        .context("selection offset overflow")?;
    let stop = start
        .checked_add(text.len())
        .context("selection length overflow")?;
    ensure!(
        content.get(start..stop) == Some(text.as_ref()),
        "selected text does not exactly match the file at the supplied row and byte column"
    );

    let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
    // A newline at the exclusive end belongs to the preceding selected row.
    let additional_rows = newlines - usize::from(text.ends_with('\n'));
    let end = row
        .checked_add(u32::try_from(additional_rows).context("selection spans too many rows")?)
        .context("selection end row overflow")?;
    Ok((row, end))
}

#[derive(Clone, Copy, Debug)]
struct Span {
    start: u32,
    count: u32,
}

impl Span {
    fn parse(value: &str, prefix: char) -> Result<Self> {
        let value = value
            .strip_prefix(prefix)
            .context("invalid hunk range prefix")?;
        let (start, count) = match value.split_once(',') {
            Some((start, count)) => (decimal(start)?, decimal(count)?),
            None => (decimal(value)?, 1),
        };
        ensure!(
            count == 0 || start > 0,
            "nonempty hunk range starts at row zero"
        );
        if count > 0 {
            start
                .checked_add(count - 1)
                .context("hunk row range overflow")?;
        }
        Ok(Self { start, count })
    }

    // Empty ranges specify the gap after `start`; nonempty ranges are one-based.
    fn bounds(self) -> (u64, u64) {
        let start = u64::from(self.start) - u64::from(self.count > 0);
        (start, start + u64::from(self.count))
    }

    fn contains(self, start: u32, end: u32) -> bool {
        self.count > 0 && start >= self.start && u64::from(end) <= self.bounds().1
    }
}

struct Hunk {
    old: Span,
    new: Span,
    old_seen: u32,
    new_seen: u32,
    changed: bool,
}

impl Hunk {
    fn parse(header: &str) -> Result<Self> {
        let header = header
            .strip_prefix("@@ ")
            .context("expected a unified hunk header")?;
        let (ranges, suffix) = header.split_once(" @@").context("incomplete hunk header")?;
        ensure!(
            suffix.is_empty() || suffix.starts_with(' '),
            "invalid hunk header suffix"
        );
        let (old, new) = ranges
            .split_once(' ')
            .context("hunk header needs two ranges")?;
        let old = Span::parse(old, '-')?;
        let new = Span::parse(new, '+')?;
        ensure!(old.count > 0 || new.count > 0, "empty hunk");
        Ok(Self {
            old,
            new,
            old_seen: 0,
            new_seen: 0,
            changed: false,
        })
    }

    fn finish(&self) -> Result<()> {
        ensure!(
            self.old_seen == self.old.count && self.new_seen == self.new.count,
            "truncated patch: hunk body does not match its declared counts"
        );
        ensure!(self.changed, "hunk contains no changes");
        Ok(())
    }

    fn add_line(&mut self, old: bool, new: bool) -> Result<()> {
        if old {
            self.old_seen = self
                .old_seen
                .checked_add(1)
                .context("old hunk count overflow")?;
        }
        if new {
            self.new_seen = self
                .new_seen
                .checked_add(1)
                .context("new hunk count overflow")?;
        }
        ensure!(
            self.old_seen <= self.old.count && self.new_seen <= self.new.count,
            "hunk body exceeds its declared counts"
        );
        self.changed |= old != new;
        Ok(())
    }
}

/// Validate the entire GitHub per-file unified patch, then require the complete
/// inclusive range to belong to one hunk's RHS. Separate hunks cannot be joined,
/// even when their RHS row numbers are adjacent.
pub fn validate_patch(patch: &str, start: u32, end: u32) -> Result<()> {
    ensure!(start > 0 && end >= start, "invalid comment row range");
    let mut hunk: Option<Hunk> = None;
    let mut covered = false;
    let mut previous_body: Option<(bool, bool)> = None;
    let mut old_ended = false;
    let mut new_ended = false;

    for (index, raw_line) in patch.split_terminator('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with("@@") {
            let next = Hunk::parse(line)
                .with_context(|| format!("invalid patch header at line {}", index + 1))?;
            if let Some(previous) = &hunk {
                previous.finish()?;
                covered |= previous.new.contains(start, end);
                ensure!(
                    next.old.bounds().0 >= previous.old.bounds().1
                        && next.new.bounds().0 >= previous.new.bounds().1,
                    "patch hunks overlap or are out of order"
                );
            }
            ensure!(
                !(old_ended && next.old.count > 0 || new_ended && next.new.count > 0),
                "hunk follows the end of a file without a newline"
            );
            hunk = Some(next);
            previous_body = None;
            continue;
        }

        let current = hunk
            .as_mut()
            .context("patch content appears before a hunk header")?;
        if line == "\\ No newline at end of file" {
            let (old, new) = previous_body
                .take()
                .context("misplaced no-newline marker")?;
            old_ended |= old;
            new_ended |= new;
            continue;
        }
        let (old, new) = match line.as_bytes().first() {
            Some(b' ') => (true, true),
            Some(b'-') => (true, false),
            Some(b'+') => (false, true),
            _ => bail!("invalid patch body at line {}", index + 1),
        };
        ensure!(
            !(old && old_ended || new && new_ended),
            "patch contains rows after a no-newline marker on the same side"
        );
        current.add_line(old, new)?;
        previous_body = Some((old, new));
    }

    let last = hunk.context("patch contains no hunks")?;
    last.finish()?;
    covered |= last.new.contains(start, end);
    ensure!(
        covered,
        "selection must fit entirely within one hunk on the RIGHT side"
    );
    Ok(())
}

pub fn validate_comment(comment: &Comment, content: &[u8], files: &[ChangedFile]) -> Result<()> {
    safe_path(&comment.path)?;
    let content = text_blob(content)?;
    let mut matches = files.iter().filter(|file| file.filename == comment.path);
    let file = matches
        .next()
        .context("comment path is not a changed file in this pull request")?;
    ensure!(
        matches.next().is_none(),
        "comment path occurs more than once in the pull request files"
    );
    ensure!(file.status != "removed", "removed files have no RIGHT side");
    let patch = file.patch.as_deref().context("changed file has no patch")?;
    let (_, end) = line_range(
        content,
        comment.start_line,
        comment.column,
        &comment.selected_text,
    )?;
    ensure!(
        end == comment.line,
        "stored end row does not match the selected text"
    );
    validate_patch(patch, comment.start_line, end)
}

pub fn make_comment(
    root: &Path,
    selection: &Selection,
    content: &[u8],
    files: &[ChangedFile],
    body: String,
) -> Result<Comment> {
    let path = repo_path(root, &selection.file)?;
    let content_text = text_blob(content)?;
    let disk = fs::read(&selection.file).context("cannot read selected file from disk")?;
    let disk_text = text_blob(&disk).context("selected file on disk is not text")?;
    ensure!(
        normalize_line_endings(disk_text)? == normalize_line_endings(content_text)?,
        "selected file on disk differs from the pull request blob; discard saved edits or refresh the review"
    );
    let (start_line, line) = line_range(
        content_text,
        selection.row,
        selection.column,
        &selection.text,
    )?;
    let comment = Comment {
        path,
        start_line,
        line,
        column: selection.column,
        selected_text: normalize_line_endings(&selection.text)?.into_owned(),
        body,
        agent: false,
    };
    validate_comment(&comment, content, files)?;
    Ok(comment)
}

/// Build and validate an inline comment from explicit repo-relative path and 1-based
/// RIGHT-side line numbers against the pinned PR head blob and cached diff patches.
pub fn make_comment_from_lines(
    path: &str,
    start_line: u32,
    line: u32,
    content: &[u8],
    files: &[ChangedFile],
    body: String,
    agent: bool,
) -> Result<Comment> {
    safe_path(path)?;
    ensure!(
        start_line > 0 && start_line <= line,
        "line range must be one-based with start_line <= line"
    );
    let content_text = text_blob(content)?;
    let normalized = normalize_line_endings(content_text)?;
    let lines: Vec<&str> = normalized.lines().collect();
    let start_idx = usize::try_from(start_line - 1).context("start_line exceeds usize")?;
    let end_idx = usize::try_from(line - 1).context("line exceeds usize")?;
    ensure!(
        end_idx < lines.len(),
        "line {line} is outside the file ({} lines)",
        lines.len()
    );
    let selected_text = lines[start_idx..=end_idx].join("\n");
    let comment = Comment {
        path: path.to_owned(),
        start_line,
        line,
        column: 1,
        selected_text,
        body,
        agent,
    };
    validate_comment(&comment, content, files)?;
    Ok(comment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn selection_values(
        file: Option<&str>,
        row: Option<&str>,
        column: Option<&str>,
        text: Option<&str>,
    ) -> Result<Selection> {
        Selection::from_values(
            file.map(Into::into),
            row.map(Into::into),
            column.map(Into::into),
            text.map(Into::into),
        )
    }

    fn changed_file(path: &str, patch: Option<&str>) -> ChangedFile {
        ChangedFile {
            filename: path.into(),
            status: "modified".into(),
            previous_filename: None,
            patch: patch.map(Into::into),
        }
    }

    fn comment() -> Comment {
        Comment {
            path: "src/example.rs".into(),
            start_line: 2,
            line: 3,
            column: 3,
            selected_text: "α\nlast\n".into(),
            body: "Please explain this.".into(),
            agent: false,
        }
    }

    const CONTENT: &[u8] = "first\néα\nlast\n".as_bytes();
    const PATCH: &str = "@@ -1,2 +1,3 @@\n first\n-old\n+éα\n+last\n";

    #[test]
    fn env_values_are_selection_start_not_reverse_cursor_endpoint() {
        let selection =
            selection_values(Some("/repo/a.rs"), Some("2"), Some("3"), Some("α\nlast\n")).unwrap();
        assert_eq!(selection.file, PathBuf::from("/repo/a.rs"));
        assert_eq!((selection.row, selection.column), (2, 3));
        assert_eq!(
            line_range(
                std::str::from_utf8(CONTENT).unwrap(),
                selection.row,
                selection.column,
                &selection.text
            )
            .unwrap(),
            (2, 3)
        );
    }

    #[test]
    fn env_values_require_all_four_variables() {
        for values in [
            (None, Some("1"), Some("1"), Some("x")),
            (Some("/repo/a"), None, Some("1"), Some("x")),
            (Some("/repo/a"), Some("1"), None, Some("x")),
            (Some("/repo/a"), Some("1"), Some("1"), None),
        ] {
            assert!(selection_values(values.0, values.1, values.2, values.3).is_err());
        }
    }

    #[test]
    fn env_values_reject_blank_text_relative_files_and_invalid_numbers() {
        for text in ["", " ", "\r\n\t", "\u{2003}\u{a0}"] {
            assert!(selection_values(Some("/repo/a"), Some("1"), Some("1"), Some(text)).is_err());
        }
        for file in ["", "a.rs", "../a.rs"] {
            assert!(selection_values(Some(file), Some("1"), Some("1"), Some("x")).is_err());
        }
        for value in ["0", "", "-1", "+1", "1.0", " 1", "1 ", "4294967296", "one"] {
            assert!(
                selection_values(Some("/repo/a"), Some(value), Some("1"), Some("x")).is_err(),
                "row {value:?}"
            );
            assert!(
                selection_values(Some("/repo/a"), Some("1"), Some(value), Some("x")).is_err(),
                "column {value:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn env_values_reject_non_utf8_numbers_and_text() {
        use std::os::unix::ffi::OsStringExt;
        for index in 1..4 {
            let mut values = [
                OsString::from("/repo/a"),
                "1".into(),
                "1".into(),
                "x".into(),
            ];
            values[index] = OsString::from_vec(vec![0xff]);
            let [file, row, column, text] = values;
            assert!(
                Selection::from_values(Some(file), Some(row), Some(column), Some(text)).is_err()
            );
        }
    }

    #[test]
    fn line_range_uses_utf8_bytes_not_characters_or_utf16_units() {
        assert_eq!(line_range("aé🦀z\n", 1, 4, "🦀z").unwrap(), (1, 1));
        assert_eq!(line_range("aé🦀z\n", 1, 8, "z").unwrap(), (1, 1));
        assert_eq!(line_range("aé🦀z\n", 1, 2, "é🦀").unwrap(), (1, 1));
        for column in [3, 5, 6, 7] {
            assert!(
                line_range("aé🦀z\n", 1, column, "z").is_err(),
                "column {column}"
            );
        }
    }

    #[test]
    fn line_range_normalizes_crlf_in_both_inputs() {
        for content in [
            "zero\néα\nlast\n",
            "zero\r\néα\r\nlast\r\n",
            "zero\r\néα\nlast\r\n",
        ] {
            for text in ["α\nlast\n", "α\r\nlast\r\n", "α\nlast\r\n"] {
                assert_eq!(line_range(content, 2, 3, text).unwrap(), (2, 3));
            }
        }
    }

    #[test]
    fn line_range_rejects_bare_cr_in_content_and_selection() {
        for content in [
            "zero\réα\rlast\r",
            "zero\réα\r\nlast\n",
            "zero\néα\nlast\r",
            "zero\néα\nlast\nextra\rline\n",
        ] {
            let error = line_range(content, 2, 3, "α\nlast\n").unwrap_err();
            assert!(error.to_string().contains("bare CR"), "{content:?}");
        }
        for text in ["α\rlast\r", "α\rlast\n", "α\nlast\r"] {
            let error = line_range("zero\néα\nlast\n", 2, 3, text).unwrap_err();
            assert!(error.to_string().contains("bare CR"), "{text:?}");
        }
    }

    #[test]
    fn line_range_handles_exclusive_end_at_newline_and_eof() {
        for (text, end) in [
            ("abc", 1),
            ("abc\n", 1),
            ("abc\n\n", 2),
            ("abc\n\nxyz", 3),
            ("abc\n\nxyz\n", 3),
        ] {
            assert_eq!(line_range("abc\n\nxyz\n", 1, 1, text).unwrap(), (1, end));
        }
        assert_eq!(line_range("abc\nxyz", 2, 2, "yz").unwrap(), (2, 2));
        assert_eq!(line_range("abc\nxyz\n", 1, 4, "\nxyz\n").unwrap(), (1, 2));
        assert_eq!(line_range("\nxyz", 1, 1, "\nxyz").unwrap(), (1, 2));
    }

    #[test]
    fn line_range_rejects_whitespace_and_requires_the_entire_exact_slice() {
        for text in ["", " ", "\t\n", "\u{2003}"] {
            assert!(line_range(" \t\n\u{2003}", 1, 1, text).is_err());
        }
        for (row, column, text) in [
            (1, 1, "abX"),
            (1, 1, "abcd"),
            (1, 2, "abc"),
            (2, 1, "abc"),
            (1, 1, "abc\nzzz"),
            (1, 1, "abc\nxyz\nmore"),
        ] {
            assert!(
                line_range("abc\nxyz\n", row, column, text).is_err(),
                "{row}:{column} {text:?}"
            );
        }
        assert!(line_range(" xyz ", 1, 1, "xyz").is_err());
        assert_eq!(line_range(" xyz ", 1, 1, " xyz ").unwrap(), (1, 1));
    }

    #[test]
    fn line_range_does_not_let_columns_skip_into_the_next_line() {
        assert!(line_range("a\ntarget\n", 1, 3, "target").is_err());
        assert!(line_range("a\r\ntarget\r\n", 1, 3, "target").is_err());
        assert!(line_range("\ntarget", 1, 2, "target").is_err());
        assert!(line_range("a\ntarget", 2, 2, "target").is_err());
    }

    #[test]
    fn line_range_rejects_zero_and_out_of_bounds_coordinates() {
        for (row, column) in [(0, 1), (1, 0), (3, 1), (1, 6), (u32::MAX, 1), (1, u32::MAX)] {
            assert!(line_range("text\n", row, column, "text").is_err());
        }
        assert!(line_range("", 1, 1, "text").is_err());
        assert!(line_range("text\n", 2, 1, "text").is_err());
        assert!(line_range("text", 1, 5, "x").is_err());
    }

    #[test]
    fn patch_supports_context_and_added_rows_but_not_deleted_coordinates() {
        let patch =
            "@@ -40,4 +10,3 @@ function name\n context\n-deleted\n-replaced\n+added\n ending";
        for (start, end) in [(10, 10), (11, 11), (12, 12), (10, 12)] {
            validate_patch(patch, start, end).unwrap();
        }
        for (start, end) in [(0, 1), (12, 10), (9, 10), (12, 13), (40, 40)] {
            assert!(validate_patch(patch, start, end).is_err());
        }
    }

    #[test]
    fn patch_supports_omitted_counts_zero_old_count_and_no_final_newline() {
        validate_patch("@@ -1 +1 @@\n-old\n+new", 1, 1).unwrap();
        validate_patch("@@ -0,0 +1,2 @@\n+first\n+second\n", 1, 2).unwrap();
        validate_patch("@@ -5,0 +6 @@\n+added", 6, 6).unwrap();
        validate_patch("@@ -1 +1 @@\r\n-old\r\n+new\r\n", 1, 1).unwrap();
        validate_patch(
            "@@ -4294967295 +4294967295 @@\n-old\n+new",
            u32::MAX,
            u32::MAX,
        )
        .unwrap();
    }

    #[test]
    fn patch_rejects_deleted_only_hunks_but_allows_rhs_context_after_deletion() {
        assert!(validate_patch("@@ -1,2 +0,0 @@\n-a\n-b", 1, 1).is_err());
        assert!(validate_patch("@@ -5 +4,0 @@\n-deleted", 4, 4).is_err());
        validate_patch("@@ -1,2 +1 @@\n-deleted\n context", 1, 1).unwrap();
        validate_patch("@@ -1 +0,0 @@\n-deleted\n@@ -4 +3 @@\n-old\n+new", 3, 3).unwrap();
    }

    #[test]
    fn patch_allows_one_hunk_of_many_but_never_combines_hunks() {
        let patch =
            "@@ -1,2 +1,2 @@\n-old\n+new\n context\n@@ -7,2 +7,2 @@\n context\n-old\n+new\n";
        validate_patch(patch, 1, 2).unwrap();
        validate_patch(patch, 7, 8).unwrap();
        for (start, end) in [(2, 7), (1, 8), (3, 3), (6, 7)] {
            assert!(validate_patch(patch, start, end).is_err());
        }
        let adjacent = "@@ -1 +1 @@\n-a\n+b\n@@ -2 +2 @@\n-c\n+d";
        validate_patch(adjacent, 1, 1).unwrap();
        validate_patch(adjacent, 2, 2).unwrap();
        assert!(validate_patch(adjacent, 1, 2).is_err());
    }

    #[test]
    fn patch_rejects_truncation_anywhere_even_after_a_covered_selection() {
        for patch in [
            "@@ -1,2 +1,2 @@\n-old\n+new",
            "@@ -1,2 +1 @@\n-old\n+new",
            "@@ -1 +1,2 @@\n-old\n+new",
            "@@ -1 +1 @@\n-old\n+new\n@@ -9 +9 @@\n-old",
            "@@ -1,2 +1,2 @@\n-old\n+new\n@@ -9 +9 @@\n-old\n+new",
            "@@ -1 +1 @@",
        ] {
            assert!(validate_patch(patch, 1, 1).is_err(), "{patch:?}");
        }
    }

    #[test]
    fn patch_rejects_excess_rows_invalid_body_and_non_hunk_input() {
        for patch in [
            "",
            "\n",
            "Binary files a and b differ",
            "diff --git a/a b/a\n@@ -1 +1 @@\n-a\n+b",
            "@@ -1 +1 @@\n-a\n+b\n+extra",
            "@@ -1 +1 @@\n-a\n+b\n-extra",
            "@@ -1 +1 @@\n-a\n+b\n extra",
            "@@ -1 +1 @@\n-a\n+b\n\n",
            "@@ -1 +1 @@\n-a\n+b\n...",
            "@@ -1 +1 @@\n-a\n+b\n\\ unknown marker",
            "@@ -1 +1 @@\n unchanged",
        ] {
            assert!(validate_patch(patch, 1, 1).is_err(), "{patch:?}");
        }
    }

    #[test]
    fn patch_rejects_malformed_headers_counts_and_overlapping_hunks() {
        for header in [
            "@@@ -1 +1 @@@",
            "@@ -1 +1",
            "@@ -1 @@",
            "@@ +1 -1 @@",
            "@@ -0 +1 @@",
            "@@ -1 +0 @@",
            "@@ -0,0 +0,0 @@",
            "@@ -1,-1 +1 @@",
            "@@ -1, +1 @@",
            "@@ -,1 +1 @@",
            "@@ -1,1,1 +1 @@",
            "@@ -1 +1,+1 @@",
            "@@ -1 +1 @@junk",
            "@@ -4294967296 +1 @@",
            "@@ -1 +4294967295,2 @@",
            "@@ -4294967295,2 +1 @@",
            "@@ -1  +1 @@",
            "@@ -1 +1 +2 @@",
        ] {
            assert!(
                validate_patch(&format!("{header}\n-old\n+new"), 1, 1).is_err(),
                "{header}"
            );
        }
        for header in ["@@ -1 +2 @@", "@@ -2 +1 @@", "@@ -1 +1 @@"] {
            assert!(
                validate_patch(&format!("@@ -1 +1 @@\n-a\n+b\n{header}\n-c\n+d"), 1, 1).is_err()
            );
        }
        assert!(validate_patch("@@ -5 +5 @@\n-a\n+b\n@@ -1 +1 @@\n-c\n+d", 5, 5).is_err());
    }

    #[test]
    fn patch_ignores_well_formed_no_newline_markers_for_counts() {
        validate_patch(
            "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file",
            1,
            1,
        )
        .unwrap();
        validate_patch(
            "@@ -1,2 +1,2 @@\n-old\n+new\n context\n\\ No newline at end of file\n",
            1,
            2,
        )
        .unwrap();
        validate_patch(
            "@@ -1 +1,2 @@\n-old\n\\ No newline at end of file\n+new\n+last\n",
            1,
            2,
        )
        .unwrap();
    }

    #[test]
    fn patch_rejects_misplaced_no_newline_markers() {
        for patch in [
            "\\ No newline at end of file\n@@ -1 +1 @@\n-a\n+b",
            "@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b",
            "@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n\\ No newline at end of file",
            "@@ -1 +1,2 @@\n-a\n+b\n\\ No newline at end of file\n+c",
            "@@ -1,2 +1 @@\n-a\n\\ No newline at end of file\n-c\n+b",
            "@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n@@ -3 +3 @@\n-c\n+d",
        ] {
            assert!(validate_patch(patch, 1, 1).is_err(), "{patch:?}");
        }
    }

    #[test]
    fn comment_requires_an_exact_unique_current_pr_path() {
        validate_comment(
            &comment(),
            CONTENT,
            &[changed_file("src/example.rs", Some(PATCH))],
        )
        .unwrap();
        for path in [
            "example.rs",
            "src/Example.rs",
            "src/example.rs.bak",
            "src/other.rs",
        ] {
            assert!(
                validate_comment(&comment(), CONTENT, &[changed_file(path, Some(PATCH))]).is_err()
            );
        }
        assert!(validate_comment(&comment(), CONTENT, &[]).is_err());
        assert!(
            validate_comment(
                &comment(),
                CONTENT,
                &[
                    changed_file("src/example.rs", Some(PATCH)),
                    changed_file("src/example.rs", Some(PATCH))
                ]
            )
            .is_err()
        );
        let mut renamed = changed_file("src/new.rs", Some(PATCH));
        renamed.status = "renamed".into();
        renamed.previous_filename = Some("src/example.rs".into());
        assert!(validate_comment(&comment(), CONTENT, &[renamed]).is_err());
        let mut renamed = changed_file("src/example.rs", Some(PATCH));
        renamed.status = "renamed".into();
        renamed.previous_filename = Some("src/old.rs".into());
        validate_comment(&comment(), CONTENT, &[renamed]).unwrap();
    }

    #[test]
    fn comment_rejects_removed_missing_and_invalid_patches() {
        let mut removed = changed_file("src/example.rs", Some(PATCH));
        removed.status = "removed".into();
        assert!(validate_comment(&comment(), CONTENT, &[removed]).is_err());
        for patch in [
            None,
            Some(""),
            Some("@@ -1 +1 @@\n-old\n+new"),
            Some("@@ -1,2 +1,3 @@\n first\n-old\n+éα"),
        ] {
            assert!(
                validate_comment(
                    &comment(),
                    CONTENT,
                    &[changed_file("src/example.rs", patch)]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn comment_rejects_non_text_blobs_and_inconsistent_stored_selection() {
        let files = [changed_file("src/example.rs", Some(PATCH))];
        for content in [
            b"first\n\xff\nlast\n".as_slice(),
            b"first\n\0\nlast\n",
            b"first\nwrong\nlast\n",
        ] {
            assert!(validate_comment(&comment(), content, &files).is_err());
        }
        for end in [0, 1, 2, 4, u32::MAX] {
            let mut candidate = comment();
            candidate.line = end;
            assert!(validate_comment(&candidate, CONTENT, &files).is_err());
        }
        for (row, column) in [(0, 3), (1, 3), (2, 0), (2, 2), (2, 4), (3, 3)] {
            let mut candidate = comment();
            candidate.start_line = row;
            candidate.column = column;
            assert!(validate_comment(&candidate, CONTENT, &files).is_err());
        }
        for text in ["", " \n", "α", "α\nwrong\n"] {
            let mut candidate = comment();
            candidate.selected_text = text.into();
            assert!(validate_comment(&candidate, CONTENT, &files).is_err());
        }
        validate_comment(&comment(), "first\r\néα\r\nlast\r\n".as_bytes(), &files).unwrap();
    }

    #[test]
    fn comment_rejects_bare_cr_in_pinned_blob() {
        let files = [changed_file("src/example.rs", Some(PATCH))];
        let error =
            validate_comment(&comment(), "first\r\néα\rlast\r\n".as_bytes(), &files).unwrap_err();
        assert!(error.to_string().contains("bare CR"));
    }

    #[test]
    fn comment_rejects_unsafe_paths_even_if_listed_in_pr() {
        for path in [
            "", "/a", "../a", "a/../b", "./a", "a/./b", "a//b", "a/", "a\\b", "C:/a", "a\0b",
            "a\nb",
        ] {
            let mut candidate = comment();
            candidate.path = path.into();
            assert!(
                validate_comment(&candidate, CONTENT, &[changed_file(path, Some(PATCH))]).is_err(),
                "{path:?}"
            );
        }
    }

    fn disk_file(content: &[u8]) -> (TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let file = root.path().join("src/example.rs");
        fs::write(&file, content).unwrap();
        (root, file)
    }

    #[test]
    fn repo_path_returns_exact_relative_utf8_path() {
        let (root, file) = disk_file(CONTENT);
        assert_eq!(repo_path(root.path(), &file).unwrap(), "src/example.rs");
        assert_eq!(
            repo_path(root.path(), &fs::canonicalize(&file).unwrap()).unwrap(),
            "src/example.rs"
        );
        let unicode = root.path().join("space é.rs");
        fs::write(&unicode, b"x").unwrap();
        assert_eq!(repo_path(root.path(), &unicode).unwrap(), "space é.rs");
    }

    #[test]
    fn repo_path_rejects_outside_traversal_missing_relative_and_directories() {
        let (root, file) = disk_file(CONTENT);
        let (outside, outside_file) = disk_file(CONTENT);
        for invalid in [
            outside_file,
            root.path().join("src/../src/example.rs"),
            root.path().join("missing"),
            root.path().join("src"),
            root.path().to_path_buf(),
            PathBuf::from("src/example.rs"),
        ] {
            assert!(
                repo_path(root.path(), &invalid).is_err(),
                "{}",
                invalid.display()
            );
        }
        assert!(repo_path(&file, &file).is_err());
        let prefix_root = outside.path().join("repo");
        let prefix_sibling = outside.path().join("repo-other");
        fs::create_dir(&prefix_root).unwrap();
        fs::create_dir(&prefix_sibling).unwrap();
        let sibling_file = prefix_sibling.join("a");
        fs::write(&sibling_file, "x").unwrap();
        assert!(repo_path(&prefix_root, &sibling_file).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn repo_path_allows_symlink_root_but_no_symlinks_below_it() {
        use std::os::unix::fs::symlink;
        let (root, file) = disk_file(CONTENT);
        let aliases = tempfile::tempdir().unwrap();
        let alias = aliases.path().join("dotfiles");
        symlink(root.path(), &alias).unwrap();
        let canonical_file = fs::canonicalize(&file).unwrap();
        assert_eq!(
            repo_path(&alias, &alias.join("src/example.rs")).unwrap(),
            "src/example.rs"
        );
        assert_eq!(
            repo_path(&alias, &canonical_file).unwrap(),
            "src/example.rs"
        );
        assert!(repo_path(root.path(), &alias.join("src/example.rs")).is_err());

        let file_alias = root.path().join("alias.rs");
        symlink(&file, &file_alias).unwrap();
        assert!(repo_path(root.path(), &file_alias).is_err());
        let directory_alias = root.path().join("alias-dir");
        symlink(root.path().join("src"), &directory_alias).unwrap();
        assert!(repo_path(root.path(), &directory_alias.join("example.rs")).is_err());
        assert!(repo_path(&alias, &alias.join("alias-dir/example.rs")).is_err());

        let (outside, outside_file) = disk_file(CONTENT);
        let external = root.path().join("external");
        symlink(outside.path(), &external).unwrap();
        assert!(repo_path(root.path(), &external.join("src/example.rs")).is_err());
        let external_file = root.path().join("external.rs");
        symlink(outside_file, &external_file).unwrap();
        assert!(repo_path(root.path(), &external_file).is_err());
        let dangling = root.path().join("dangling");
        symlink(root.path().join("absent"), &dangling).unwrap();
        assert!(repo_path(root.path(), &dangling).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn repo_path_rejects_non_utf8_and_unsafe_filenames() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        for name in ["a\\b", "a\nb"] {
            let file = root.path().join(name);
            fs::write(&file, "x").unwrap();
            assert!(repo_path(root.path(), &file).is_err());
        }
        let invalid = root.path().join(OsString::from_vec(vec![0xff]));
        // Some filesystems (including APFS) reject this name at creation time.
        // Whether it exists or not, it must never yield a repository path.
        let _ = fs::write(&invalid, "x");
        assert!(repo_path(root.path(), &invalid).is_err());
    }

    #[test]
    fn make_comment_preserves_body_and_uses_normalized_selection_and_disk_text() {
        let (root, file) = disk_file("first\r\néα\r\nlast\r\n".as_bytes());
        let selection = Selection {
            file,
            row: 2,
            column: 3,
            text: "α\r\nlast\r\n".into(),
        };
        let files = [changed_file("src/example.rs", Some(PATCH))];
        let made = make_comment(
            root.path(),
            &selection,
            CONTENT,
            &files,
            "  A comment.\n".into(),
        )
        .unwrap();
        assert_eq!(made.path, "src/example.rs");
        assert_eq!((made.start_line, made.line, made.column), (2, 3, 3));
        assert_eq!(made.selected_text, "α\nlast\n");
        assert_eq!(made.body, "  A comment.\n");
        validate_comment(&made, CONTENT, &files).unwrap();
        make_comment(
            root.path(),
            &selection,
            "first\néα\r\nlast\r\n".as_bytes(),
            &files,
            "body".into(),
        )
        .unwrap();
    }

    #[test]
    fn make_comment_rejects_bare_cr_in_pinned_or_saved_content() {
        let mixed = "a\rb\nc\nd\n";
        let lf = "a\nb\nc\nd\n";
        let mixed_patch = "@@ -0,0 +1,3 @@\n+a\rb\n+c\n+d\n";
        let lf_patch = "@@ -0,0 +1,4 @@\n+a\n+b\n+c\n+d\n";
        let (root, file) = disk_file(lf.as_bytes());
        let selection = Selection {
            file: file.clone(),
            row: 2,
            column: 1,
            text: "b".into(),
        };
        for (blob, disk, patch) in [
            (mixed, lf, mixed_patch),
            (lf, mixed, lf_patch),
            (mixed, mixed, mixed_patch),
        ] {
            fs::write(&file, disk).unwrap();
            let files = [changed_file("src/example.rs", Some(patch))];
            let error = make_comment(
                root.path(),
                &selection,
                blob.as_bytes(),
                &files,
                "Comment on b, not c".into(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("bare CR"), "{blob:?}, {disk:?}");
        }
    }

    #[test]
    fn make_comment_rejects_saved_edits_even_outside_the_selection() {
        let (root, file) = disk_file("changed\néα\nlast\n".as_bytes());
        let selection = Selection {
            file: file.clone(),
            row: 2,
            column: 3,
            text: "α\nlast\n".into(),
        };
        let files = [changed_file("src/example.rs", Some(PATCH))];
        for disk in [
            "changed\néα\nlast\n",
            "first\néα\nlast",
            "first\néα\nlast\nextra\n",
        ] {
            fs::write(&file, disk).unwrap();
            let error =
                make_comment(root.path(), &selection, CONTENT, &files, "body".into()).unwrap_err();
            assert!(error.to_string().contains("differs"));
        }
    }

    #[test]
    fn make_comment_rejects_non_text_disk_or_blob_and_invalid_selection() {
        let (root, file) = disk_file(CONTENT);
        let mut selection = Selection {
            file: file.clone(),
            row: 2,
            column: 3,
            text: "α\nlast\n".into(),
        };
        let files = [changed_file("src/example.rs", Some(PATCH))];
        for bytes in [b"\xff".as_slice(), b"\0"] {
            assert!(make_comment(root.path(), &selection, bytes, &files, "body".into()).is_err());
            fs::write(&file, bytes).unwrap();
            assert!(make_comment(root.path(), &selection, CONTENT, &files, "body".into()).is_err());
        }
        fs::write(&file, CONTENT).unwrap();
        assert!(make_comment(root.path(), &selection, CONTENT, &[], "body".into()).is_err());
        selection.text = "wrong".into();
        assert!(make_comment(root.path(), &selection, CONTENT, &files, "body".into()).is_err());
        selection.text = " \n".into();
        assert!(make_comment(root.path(), &selection, CONTENT, &files, "body".into()).is_err());
    }

    #[test]
    fn make_comment_from_lines_extracts_selected_text_and_validates_hunk() {
        let files = [changed_file("src/example.rs", Some(PATCH))];
        let comment = make_comment_from_lines(
            "src/example.rs",
            2,
            3,
            CONTENT,
            &files,
            "Check this logic.".into(),
            true,
        )
        .unwrap();
        assert_eq!(comment.path, "src/example.rs");
        assert_eq!(comment.start_line, 2);
        assert_eq!(comment.line, 3);
        assert_eq!(comment.column, 1);
        assert_eq!(comment.selected_text, "éα\nlast");
        assert_eq!(comment.body, "Check this logic.");
        assert!(comment.agent);

        // Out-of-hunk line is rejected.
        let narrow_patch = "@@ -2,1 +2,2 @@\n-old\n+éα\n+last\n";
        let narrow_files = [changed_file("src/example.rs", Some(narrow_patch))];
        assert!(
            make_comment_from_lines(
                "src/example.rs",
                1,
                1,
                CONTENT,
                &narrow_files,
                "note".into(),
                false,
            )
            .is_err()
        );
        // Out-of-bounds line is rejected.
        assert!(
            make_comment_from_lines(
                "src/example.rs",
                1,
                99,
                CONTENT,
                &files,
                "note".into(),
                false,
            )
            .is_err()
        );
        // Unsafe path is rejected.
        assert!(
            make_comment_from_lines("../example.rs", 2, 2, CONTENT, &files, "note".into(), false,)
                .is_err()
        );
    }
}
