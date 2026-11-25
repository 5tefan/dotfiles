//! Read-only snapshots of the user's `jj l`, with semantic links instead of terminal URLs.
//!
//! Polls use `--ignore-working-copy`: unsnapshotted file changes are deliberately not
//! reflected in the recorded commit ID. `--at-operation=@` also prevents a poll from
//! merging concurrent operation heads; jj reports that condition until a normal jj
//! command reconciles it. A change ID, unlike a commit ID, survives content snapshots.
//!
//! Edits take the review workspace lock when that state directory exists. Other jj
//! clients do not take that lock: the final revset rechecks both the target and the
//! current workspace's change, and normal jj provides its own working-copy locking.
//! This is not a repository-wide transaction across the validation subprocesses.
//!
//! Parsing is for line-oriented log output, not terminal emulation: cursor motion,
//! OSC payloads and unsupported SGR attributes are discarded. Tabs use eight-column
//! stops; CR, LF and CRLF are line breaks. IDs assume jj's Git backend (32/40 digits).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail, ensure};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;
use unicode_width::UnicodeWidthStr;
use vte::{Params, Perform};

use crate::ci_status::CiBadge;
use crate::github::workspace_git_dir;
use crate::review_stack::{ReviewStatus, status_map};
use crate::store::{Store, prepare_directory};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub change_id: String,
    pub commit_id: String,
    pub immutable: bool,
    pub current: bool,
    pub divergent: bool,
}

#[derive(Clone, Debug)]
pub struct RevisionSpan {
    pub row: usize,
    /// Zero-based, half-open terminal display columns, not byte or character indices.
    pub columns: Range<usize>,
    pub revision: Revision,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub lines: Vec<Line<'static>>,
    /// Includes immutable revisions and each visible occurrence of an ID.
    /// Use `revision.change_id` for anchors; `commit_id` changes after snapshots.
    pub revisions: Vec<RevisionSpan>,
}

const CHANGE_ALIAS: &str = "format_short_change_id_with_change_offset(commit)";
const ORIGINAL_ALIAS: &str = "__zed_pr_log_original_change_id(commit)";
const ROOT_ALIAS: &str = "format_root_commit(root)";
const ROOT_ID_ALIAS: &str = "__zed_pr_log_root_change_id(root)";
const METADATA: &str = r#"json({
    "change_id" => commit.change_id(),
    "commit_id" => commit.commit_id(),
    "immutable" => commit.immutable(),
    "current" => commit.current_working_copy(),
    "divergent" => commit.divergent(),
})"#;

/// Preserve jj's graph, template, offsets and labels; only insert private OSC 8 tags.
/// Custom log templates must use jj's change-ID formatting aliases to expose links.
/// Templates that print IDs directly cannot be mapped reliably from display text.
pub fn load(root: &Path) -> Result<Snapshot> {
    let original = template_alias(root, CHANGE_ALIAS)?;
    let root_format = template_alias(root, ROOT_ALIAS)?;
    // The built-in root formatter bypasses CHANGE_ALIAS. Preserve its short-ID
    // expression verbatim, rather than imposing the normal commit's offset format.
    let root_id = "format_short_change_id(root.change_id())";
    let instrumented_root = root_format.replace(root_id, "__zed_pr_log_root_change_id(root)");
    let mut command = jj(root, true);
    command.args(["--config", &alias_config(ORIGINAL_ALIAS, &original)?]);
    command.args([
        "--config",
        &alias_config(
            CHANGE_ALIAS,
            &linked("commit", "__zed_pr_log_original_change_id(commit)"),
        )?,
        "--config",
        &alias_config(ROOT_ALIAS, &instrumented_root)?,
        "--config",
        &alias_config(ROOT_ID_ALIAS, &linked("root", root_id))?,
        "l",
        "--color=always",
    ]);
    let output = run(&mut command, "load jj l (the l alias must run jj log)")?;
    let mut snapshot = parse(&output.stdout);
    let statuses = status_map(&root.join("scratch/zed-pr-review"));
    attach_review_badges(&mut snapshot, &statuses);
    Ok(snapshot)
}

fn attach_review_badges(snapshot: &mut Snapshot, statuses: &HashMap<String, ReviewStatus>) {
    if statuses.is_empty() {
        return;
    }
    let mut annotated_rows = HashSet::new();
    for span in &snapshot.revisions {
        if !annotated_rows.insert(span.row) {
            continue;
        }
        let Some(status) = statuses.get(&span.revision.change_id) else {
            continue;
        };
        let badge = format_review_badge(&span.revision, status);
        if let Some(badge) = badge
            && let Some(line) = snapshot.lines.get_mut(span.row)
        {
            line.spans.push(Span::raw(" "));
            line.spans.push(badge);
        }
    }
}

fn format_review_badge(revision: &Revision, status: &ReviewStatus) -> Option<Span<'static>> {
    if revision.commit_id != status.expected_commit_id {
        return Some(Span::styled(
            "[! modified]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(state) = status.submitted_state.as_deref() {
        let (label, color) = match state {
            "APPROVED" => ("[✓ approved]", Color::Green),
            "COMMENTED" => ("[✓ commented]", Color::Cyan),
            "CHANGES_REQUESTED" => ("[✗ changes requested]", Color::Red),
            _ => ("[✓ submitted]", Color::Green),
        };
        return Some(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    if status.pending_comments > 0 {
        return Some(Span::styled(
            format!("[{} pending]", status.pending_comments),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    None
}

pub fn format_ci_badge(badge: CiBadge) -> Span<'static> {
    let (label, color) = match badge {
        CiBadge::Passing => ("✓", Color::Green),
        CiBadge::OptionalFailed => ("!", Color::Yellow),
        CiBadge::RequiredFailed => ("✗", Color::Red),
    };
    Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// Edit only the same, still-visible, unambiguous mutable commit that was clicked.
/// A normal jj edit snapshots local work and honors immutable protection. It can
/// therefore snapshot work even when its subsequent stale-target check rejects an edit.
pub fn edit(root: &Path, target: &Revision) -> Result<String> {
    validate_identity(target)?;
    let state = root.join("scratch/zed-pr-review");
    let _workspace = match state.symlink_metadata() {
        Ok(_) => {
            let git_dir = workspace_git_dir(root)?;
            let directory = prepare_directory(root, &git_dir, &state, None)?;
            Some(Store::at(&directory.join("workspace"))?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).context("Cannot inspect the review workspace lock directory");
        }
    };

    // Read both in one repository view. Never use a short ID, a bookmark, or the
    // snapshot's `current` flag to decide which commit is now being edited.
    let revset = format!(
        "(change_id(\"{}\") & ::visible_heads()) | @",
        target.change_id
    );
    let template = format!("{} ++ \"\\n\"", METADATA.replace("commit.", "self."));
    let output = run(
        jj(root, true).args(["log", "--no-graph", "-r", &revset, "-T", &template]),
        "revalidate the clicked change and current workspace",
    )?;
    let rows =
        std::str::from_utf8(&output.stdout).context("jj returned non-UTF-8 revision metadata")?;
    let revisions = rows
        .lines()
        .map(|line| {
            let revision: Revision =
                serde_json::from_str(line).context("jj returned invalid revision metadata")?;
            validate_identity(&revision)?;
            Ok(revision)
        })
        .collect::<Result<Vec<_>>>()?;
    let matches: Vec<_> = revisions
        .iter()
        .filter(|r| r.change_id == target.change_id)
        .collect();
    ensure!(
        matches.len() == 1,
        "The clicked change is no longer visible or is ambiguous; refresh the log"
    );
    let resolved = matches[0];
    ensure!(
        resolved.commit_id == target.commit_id,
        "The clicked change was rewritten; refresh the log before editing it"
    );
    ensure!(
        !resolved.divergent && !target.divergent,
        "The clicked change is divergent; resolve its divergence before editing it"
    );
    let current = revisions
        .iter()
        .find(|r| r.current)
        .context("The workspace has no current revision; refresh the log")?;
    if resolved.current {
        return Ok(format!("Already editing {}", resolved.change_id));
    }
    ensure!(
        !resolved.immutable && !target.immutable,
        "The clicked change is immutable; it cannot be edited"
    );

    let revset = edit_revset(resolved, current);
    run(
        jj(root, false).args(["edit", "--", &revset]),
        "edit the clicked change (it may have changed, become immutable, or the workspace may have moved; refresh the log)",
    )?;
    Ok(format!("Editing {}", resolved.change_id))
}

fn edit_revset(target: &Revision, current: &Revision) -> String {
    // `exactly` must wrap the change resolution BEFORE the commit intersection:
    // selecting one commit out of a divergent change must not disambiguate it.
    // The second term is empty, but errors if another command switched workspaces
    // between validation and jj edit. Check the change, not its content hash, so
    // jj edit can snapshot dirty files normally before evaluating this expression.
    format!(
        "exactly((exactly(change_id(\"{}\") & ::visible_heads(), 1) & commit_id(\"{}\")) ~ divergent(), 1) | (exactly(@ & change_id(\"{}\"), 1) ~ @)",
        target.change_id, target.commit_id, current.change_id,
    )
}

fn validate_identity(revision: &Revision) -> Result<()> {
    ensure!(
        revision.change_id.len() == 32
            && revision
                .change_id
                .bytes()
                .all(|b| (b'k'..=b'z').contains(&b)),
        "Expected a full jj change ID, not a prefix or revset"
    );
    ensure!(
        revision.commit_id.len() == 40
            && revision
                .commit_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Expected a full Git commit ID, not a prefix or revset"
    );
    Ok(())
}

fn linked(argument: &str, body: &str) -> String {
    let metadata = METADATA.replace("commit.", &format!("{argument}."));
    // hyperlink() may encode a URL. Raw escapes keep the JSON byte-for-byte and
    // leave all labels on the original template intact (verified with jj 0.47.1).
    format!(
        r#"raw_escape_sequence("\e]8;;jj-log:" ++ {metadata} ++ "\e\\") ++ {body} ++ raw_escape_sequence("\e]8;;\e\\")"#
    )
}

fn alias_config(name: &str, template: &str) -> Result<String> {
    // JSON string quoting is also valid TOML basic-string quoting here. Do not
    // trim/parse/reconstruct the original template: preserve custom coalescing.
    Ok(format!(
        "template-aliases.{}={}",
        serde_json::to_string(name)?,
        serde_json::to_string(template)?
    ))
}

fn template_alias(root: &Path, name: &str) -> Result<String> {
    let key = format!("template-aliases.{}", serde_json::to_string(name)?);
    let output = run(
        jj(root, true).args(["config", "get", &key]),
        &format!("read jj template alias {name}"),
    )?;
    let mut value = String::from_utf8(output.stdout).context("jj template alias is not UTF-8")?;
    // config get adds a newline; any newline inside the configured value stays.
    if value.ends_with('\n') {
        value.pop();
    }
    Ok(value)
}

// github's builders are private. Keep its Git-context clearing and its normal
// workspace safety settings, without applying --ignore-working-copy to edits.
fn jj(root: &Path, read_only: bool) -> Command {
    let mut command = Command::new("jj");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(root)
        .env("JJ_PAGER", "cat")
        .env("PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["--no-pager", "--color=never"]);
    if read_only {
        command.args(["--ignore-working-copy", "--at-operation=@"]);
    } else {
        command.args([
            "--config",
            "snapshot.auto-update-stale=false",
            "--config",
            "git.abandon-unreachable-commits=false",
        ]);
    }
    command
}

fn run(command: &mut Command, action: &str) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("Cannot {action}"))?;
    if !output.status.success() {
        let stderr = plain(&output.stderr);
        let stdout = plain(&output.stdout);
        bail!(
            "Cannot {action} ({}): {}",
            output.status,
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    Ok(output)
}

fn plain(bytes: &[u8]) -> String {
    parse(bytes)
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn parse(bytes: &[u8]) -> Snapshot {
    let mut terminal = LogParser::default();
    vte::Parser::new().advance(&mut terminal, bytes);
    if !terminal.text.is_empty() {
        terminal.line_break();
    }
    terminal.snapshot
}

struct LogParser {
    snapshot: Snapshot,
    text: String,
    buffer: String,
    spans: Vec<Span<'static>>,
    style: Style,
    active: Option<Revision>,
    link_start: usize,
    links: Vec<(Range<usize>, Revision)>,
    carriage_return: bool,
}

impl Default for LogParser {
    fn default() -> Self {
        Self {
            snapshot: Snapshot {
                lines: Vec::new(),
                revisions: Vec::new(),
            },
            text: String::new(),
            buffer: String::new(),
            spans: Vec::new(),
            style: Style::default(),
            active: None,
            link_start: 0,
            links: Vec::new(),
            carriage_return: false,
        }
    }
}

impl LogParser {
    fn flush_text(&mut self) {
        if !self.buffer.is_empty() {
            self.spans
                .push(Span::styled(std::mem::take(&mut self.buffer), self.style));
        }
    }

    fn flush_link(&mut self) {
        if let Some(revision) = &self.active
            && self.link_start < self.text.len()
        {
            self.links
                .push((self.link_start..self.text.len(), revision.clone()));
        }
        self.link_start = self.text.len();
    }

    fn line_break(&mut self) {
        self.flush_link();
        self.flush_text();
        let row = self.snapshot.lines.len();
        for (bytes, revision) in self.links.drain(..) {
            // Whole-prefix widths handle combining marks and emoji ZWJ sequences,
            // including sequences split by SGR labels. Per-char widths do not.
            let start = self.text[..bytes.start].width();
            let end = self.text[..bytes.end].width();
            if start < end {
                self.snapshot.revisions.push(RevisionSpan {
                    row,
                    columns: start..end,
                    revision,
                });
            }
        }
        self.snapshot
            .lines
            .push(Line::from(std::mem::take(&mut self.spans)));
        self.text.clear();
        self.link_start = 0;
    }

    fn sgr(&mut self, params: &Params) {
        self.flush_text();
        let groups: Vec<_> = params.iter().collect();
        let mut index = 0;
        while index < groups.len() {
            let group = groups[index];
            let code = group[0];
            match code {
                0 => self.style = Style::default(),
                1 => self.style = self.style.add_modifier(Modifier::BOLD),
                2 => self.style = self.style.add_modifier(Modifier::DIM),
                3 => self.style = self.style.add_modifier(Modifier::ITALIC),
                4 | 21 => self.style = self.style.add_modifier(Modifier::UNDERLINED),
                5 => self.style = self.style.add_modifier(Modifier::SLOW_BLINK),
                6 => self.style = self.style.add_modifier(Modifier::RAPID_BLINK),
                7 => self.style = self.style.add_modifier(Modifier::REVERSED),
                8 => self.style = self.style.add_modifier(Modifier::HIDDEN),
                9 => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
                22 => self.style = self.style.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => self.style = self.style.remove_modifier(Modifier::ITALIC),
                24 => self.style = self.style.remove_modifier(Modifier::UNDERLINED),
                25 => {
                    self.style = self
                        .style
                        .remove_modifier(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK)
                }
                27 => self.style = self.style.remove_modifier(Modifier::REVERSED),
                28 => self.style = self.style.remove_modifier(Modifier::HIDDEN),
                29 => self.style = self.style.remove_modifier(Modifier::CROSSED_OUT),
                30..=37 => self.style.fg = Some(ansi_color(code - 30)),
                40..=47 => self.style.bg = Some(ansi_color(code - 40)),
                90..=97 => self.style.fg = Some(ansi_color(code - 90 + 8)),
                100..=107 => self.style.bg = Some(ansi_color(code - 100 + 8)),
                39 => self.style.fg = None,
                49 => self.style.bg = None,
                38 | 48 | 58 => {
                    let color = if group.len() > 1 {
                        colon_color(group)
                    } else {
                        semicolon_color(&groups, &mut index)
                    };
                    if let Some(color) = color {
                        match code {
                            38 => self.style.fg = Some(color),
                            48 => self.style.bg = Some(color),
                            // Underline colors have no representation with the
                            // agreed ratatui features; still consume their values.
                            _ => (),
                        }
                    }
                }
                _ => (),
            }
            index += 1;
        }
    }
}

impl Perform for LogParser {
    fn print(&mut self, character: char) {
        if !character.is_control() {
            self.carriage_return = false;
            self.text.push(character);
            self.buffer.push(character);
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => {
                self.line_break();
                self.carriage_return = true;
            }
            b'\n' => {
                if !self.carriage_return {
                    self.line_break();
                }
                self.carriage_return = false;
            }
            b'\t' => {
                let count = 8 - self.text.width() % 8;
                for _ in 0..count {
                    self.print(' ');
                }
            }
            _ => (),
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.first().copied() != Some(b"8".as_slice()) {
            return;
        }
        self.flush_link();
        self.active = None;
        // An unrelated, malformed or closing link ends the preceding link too.
        // Never expose any OSC payload (including arbitrary URLs) to the terminal.
        if params.len() != 3 || params[2].len() > 1024 {
            return;
        }
        if let Some(json) = params[2].strip_prefix(b"jj-log:")
            && let Ok(revision) = serde_json::from_slice::<Revision>(json)
            && validate_identity(&revision).is_ok()
        {
            self.active = Some(revision);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action == 'm' && intermediates.is_empty() && !ignore {
            self.sgr(params);
        }
    }
}

fn ansi_color(index: u16) -> Color {
    [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ][usize::from(index)]
}

fn rgb(values: &[u16]) -> Option<Color> {
    Some(Color::Rgb(
        u8::try_from(*values.first()?).ok()?,
        u8::try_from(*values.get(1)?).ok()?,
        u8::try_from(*values.get(2)?).ok()?,
    ))
}

fn colon_color(group: &[u16]) -> Option<Color> {
    match group.get(1) {
        Some(5) if group.len() == 3 => Some(Color::Indexed(u8::try_from(group[2]).ok()?)),
        Some(2) if group.len() == 5 => rgb(&group[2..]),
        Some(2) if group.len() == 6 && group[2] == 0 => rgb(&group[3..]),
        _ => None,
    }
}

fn semicolon_color(groups: &[&[u16]], index: &mut usize) -> Option<Color> {
    *index += 1;
    let mode = *groups.get(*index)?.first()?;
    let count = match mode {
        5 => 1,
        2 => 3,
        _ => return None,
    };
    let start = *index + 1;
    *index += count;
    let values = groups.get(start..start + count)?;
    if values.iter().any(|group| group.len() != 1) {
        return None;
    }
    if mode == 5 {
        Some(Color::Indexed(u8::try_from(values[0][0]).ok()?))
    } else {
        rgb(&[values[0][0], values[1][0], values[2][0]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn revision() -> Revision {
        Revision {
            change_id: "k".repeat(32),
            commit_id: "a".repeat(40),
            immutable: false,
            current: true,
            divergent: false,
        }
    }

    fn open(revision: &Revision) -> String {
        format!(
            "\x1b]8;;jj-log:{}\x1b\\",
            serde_json::json!({
                "change_id": revision.change_id, "commit_id": revision.commit_id,
                "immutable": revision.immutable, "current": revision.current, "divergent": revision.divergent,
            })
        )
    }

    const CLOSE: &str = "\x1b]8;;\x1b\\";

    fn text(snapshot: &Snapshot) -> Vec<String> {
        snapshot
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn linked_text(snapshot: &Snapshot, span: &RevisionSpan) -> String {
        let line = text(snapshot).remove(span.row);
        line.char_indices()
            .filter(|(byte, _)| span.columns.contains(&line[..*byte].width()))
            .map(|(_, character)| character)
            .collect()
    }

    #[test]
    fn graph_colors_offsets_and_display_columns() {
        let revision = Revision {
            immutable: true,
            divergent: true,
            ..revision()
        };
        let snapshot = parse(
            format!(
                "│ 界e\u{301}👩‍💻\t{}\x1b[1;35mk\x1b[38;5;8mkkkkkkk/1{CLOSE}\x1b[0m rest\n",
                open(&revision)
            )
            .as_bytes(),
        );
        assert_eq!(text(&snapshot), ["│ 界e\u{301}👩‍💻 kkkkkkkk/1 rest"]);
        assert_eq!(snapshot.revisions.len(), 1);
        let link = &snapshot.revisions[0];
        assert_eq!(link.row, 0);
        assert_eq!(link.columns, 8..18);
        assert_eq!(link.revision, revision);
        assert_eq!(snapshot.lines[0].spans[1].style.fg, Some(Color::Magenta));
        assert!(
            snapshot.lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(snapshot.lines[0].spans[2].style.fg, Some(Color::Indexed(8)));
        assert_eq!(
            snapshot.lines[0].spans.last().unwrap().style,
            Style::default()
        );
    }

    #[test]
    fn links_across_lines_crlf_tabs_and_multiple_occurrences() {
        let mut second = revision();
        second.current = false;
        second.change_id = "z".repeat(32);
        let snapshot = parse(
            format!(
                "{}kk\r\nkk{CLOSE}\t{}zz{CLOSE}\rnext\n\n",
                open(&revision()),
                open(&second)
            )
            .as_bytes(),
        );
        assert_eq!(text(&snapshot), ["kk", "kk      zz", "next", ""]);
        assert_eq!(
            snapshot
                .revisions
                .iter()
                .map(|s| (s.row, s.columns.clone()))
                .collect::<Vec<_>>(),
            [(0, 0..2), (1, 0..2), (1, 8..10)]
        );
        assert_eq!(snapshot.revisions[2].revision, second);
        assert!(parse(b"").lines.is_empty());
        assert_eq!(text(&parse(b"a\n")), ["a"]);
        assert_eq!(text(&parse(b"a")), ["a"]);
    }

    #[test]
    fn metadata_and_all_terminal_controls_stay_internal() {
        let malformed = [
            "{}".to_owned(),
            "not json".to_owned(),
            format!(
                r#"{{"change_id":"short","commit_id":"{}","immutable":false,"current":false,"divergent":false}}"#,
                "a".repeat(40)
            ),
        ];
        for json in malformed {
            let snapshot = parse(format!("\x1b]8;;jj-log:{json}\x07id{CLOSE}").as_bytes());
            assert_eq!(text(&snapshot), ["id"]);
            assert!(snapshot.revisions.is_empty());
        }
        let snapshot = parse(format!("{}id\x1b]8;;https://example.invalid/secret\x07url{CLOSE}\x1b]52;c;c2VjcmV0\x07\x1b[2J\x1b[99C\x1bPprivate\x1b\\\x07\x08!\x1b]8;;unterminated", open(&revision())).as_bytes());
        assert_eq!(text(&snapshot), ["idurl!"]);
        assert_eq!(snapshot.revisions.len(), 1);
        assert_eq!(snapshot.revisions[0].columns, 0..2);
        assert!(!text(&snapshot)[0].chars().any(char::is_control));
    }

    #[test]
    fn sgr_truecolor_indexed_resets_and_malformed_values() {
        let snapshot = parse(b"\x1b[38;2;1;2;3;48;5;200mA\x1b[38:2::4:5:6;48:5:7mB\x1b[1;2;3;4;7;9mC\x1b[22;23;24;27;29;39;49mD\x1b[38;2;999;1;2mE\x1b[0mF");
        let spans = &snapshot.lines[0].spans;
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(spans[0].style.bg, Some(Color::Indexed(200)));
        assert_eq!(spans[1].style.fg, Some(Color::Rgb(4, 5, 6)));
        assert_eq!(spans[1].style.bg, Some(Color::Indexed(7)));
        assert!(spans[2].style.add_modifier.contains(
            Modifier::BOLD
                | Modifier::DIM
                | Modifier::ITALIC
                | Modifier::UNDERLINED
                | Modifier::REVERSED
                | Modifier::CROSSED_OUT
        ));
        assert!(spans[3].style.add_modifier.is_empty());
        assert_eq!(spans[3].style.fg, None);
        assert_eq!(spans[3].style.bg, None);
        assert_eq!(spans[4].style.fg, None);
        assert!(spans[4].style.add_modifier.is_empty());
        assert_eq!(spans[5].style, Style::default());
    }

    #[test]
    fn full_id_validation_rejects_revsets_prefixes_and_injection() {
        validate_identity(&revision()).unwrap();
        for id in [
            "@",
            "all()",
            "kkkkkkkk",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz;",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(
                validate_identity(&Revision {
                    change_id: id.into(),
                    ..revision()
                })
                .is_err()
            );
        }
        assert!(
            validate_identity(&Revision {
                commit_id: "abc123".into(),
                ..revision()
            })
            .is_err()
        );
    }

    // Run the public API in a separate test process: changing JJ_CONFIG/HOME in
    // parallel Rust tests would race, and inherited repo/editor settings are unsafe.
    #[test]
    fn isolated_real_jj() -> Result<()> {
        match Command::new("jj")
            .arg("--version")
            .stdin(Stdio::null())
            .output()
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Skipping real jj tests: jj is not installed");
                return Ok(());
            }
            result => ensure!(result?.status.success(), "Cannot run jj --version"),
        }
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("jj.toml");
        let settings = "[user]\nname = 'Log Test'\nemail = 'log@example.invalid'\n[aliases]\nl = ['log', '-r', 'all()']\n[revset-aliases]\n'immutable_heads()' = 'root()'\n";
        fs::write(&config, settings)?;
        let module = module_path!()
            .split_once("::")
            .context("test module path")?
            .1;
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                &format!("{module}::real_jj_child"),
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").context("PATH is not set")?)
            .env("HOME", directory.path())
            .env("XDG_CONFIG_HOME", directory.path())
            .env("TMPDIR", directory.path())
            .env("JJ_CONFIG", &config)
            .env("JJ_PAGER", "cat")
            .env("PAGER", "cat")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("TERM", "xterm-256color")
            .env("ZED_LOG_DATA_TEST_CHILD", "1")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .output()?;
        ensure!(
            output.status.success(),
            "Real jj tests failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_to_string(config)?, settings);
        Ok(())
    }

    fn command(root: &Path, args: &[&str]) -> Result<String> {
        let output = run(
            jj(root, false).args(args),
            "run isolated jj fixture command",
        )?;
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn current(root: &Path) -> Result<Revision> {
        load(root)?
            .revisions
            .into_iter()
            .find(|s| s.revision.current)
            .map(|s| s.revision)
            .context("No @ link in jj l")
    }

    fn operation(root: &Path) -> Result<String> {
        let output = run(
            jj(root, true).args(["op", "log", "--no-graph", "--limit", "1", "-T", "id"]),
            "read isolated jj operation",
        )?;
        Ok(String::from_utf8(output.stdout)?)
    }

    #[test]
    fn real_jj_child() -> Result<()> {
        if std::env::var_os("ZED_LOG_DATA_TEST_CHILD").is_none() {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        command(root, &["git", "init"])?;
        command(root, &["describe", "-m", "parent 界"])?;
        let parent = current(root)?;
        command(root, &["new", "-m", "current"])?;
        let original_current = current(root)?;
        let original_alias = template_alias(root, CHANGE_ALIAS)?;
        let snapshot = load(root)?;
        assert!(
            snapshot
                .revisions
                .iter()
                .any(|s| s.revision.immutable && s.revision.commit_id == "0".repeat(40))
        );
        let normal = run(
            jj(root, true).args(["--color=always", "l"]),
            "read unmodified jj l",
        )?;
        assert_eq!(snapshot.lines, parse(&normal.stdout).lines);
        assert_eq!(template_alias(root, CHANGE_ALIAS)?, original_alias);
        assert!(snapshot.revisions.iter().all(|span| {
            let id = linked_text(&snapshot, span);
            !id.is_empty() && span.revision.change_id.starts_with(&id)
        }));
        let alias_key = format!("template-aliases.{}", serde_json::to_string(CHANGE_ALIAS)?);
        let custom = format!("label(\"error\", ({original_alias}))");
        command(root, &["config", "set", "--repo", &alias_key, &custom])?;
        let custom_snapshot = load(root)?;
        let normal = run(
            jj(root, true).args(["l", "--color=always"]),
            "read custom jj l",
        )?;
        assert_eq!(custom_snapshot.lines, parse(&normal.stdout).lines);
        assert_eq!(template_alias(root, CHANGE_ALIAS)?, custom);
        command(
            root,
            &["config", "set", "--repo", &alias_key, &original_alias],
        )?;

        // Polling never snapshots files. A later content snapshot changes the
        // commit hash, but @ remains attached to the same full change identity.
        fs::write(root.join("work.txt"), "local work\n")?;
        let before = operation(root)?;
        assert_eq!(current(root)?, original_current);
        assert_eq!(operation(root)?, before);
        command(root, &["status"])?;
        let snapped = current(root)?;
        assert_eq!(snapped.change_id, original_current.change_id);
        assert_ne!(snapped.commit_id, original_current.commit_id);
        assert!(edit(root, &original_current).is_err());
        let before = operation(root)?;
        assert!(edit(root, &snapped)?.starts_with("Already editing"));
        assert_eq!(operation(root)?, before);

        fs::write(root.join("work.txt"), "more local work\n")?;
        assert!(edit(root, &parent)?.starts_with("Editing"));
        assert_eq!(current(root)?.commit_id, parent.commit_id);
        assert_eq!(
            command(
                root,
                &["file", "show", "-r", &snapped.change_id, "work.txt"]
            )?,
            "more local work"
        );

        // Ignore the old snapshot's current flag after an external workspace move.
        let still_parent = current(root)?;
        command(root, &["new", "-m", "external switch"])?;
        assert!(edit(root, &still_parent)?.starts_with("Editing"));
        command(root, &["new", "-m", "guard source"])?;
        let guard_current = current(root)?;
        let guard = edit_revset(&still_parent, &guard_current);
        command(root, &["new", "-m", "another external switch"])?;
        let moved = current(root)?;
        assert!(
            run(
                jj(root, false).args(["edit", "--", &guard]),
                "check external workspace guard"
            )
            .is_err()
        );
        assert_eq!(current(root)?.change_id, moved.change_id);

        // Stale commit identity, hidden changes, newly immutable targets, and
        // mismatched full IDs all fail without switching the working copy.
        command(
            root,
            &[
                "describe",
                "-r",
                &parent.change_id,
                "-m",
                "rewritten parent",
            ],
        )?;
        let before_change = current(root)?.change_id;
        assert!(edit(root, &parent).is_err());
        assert_eq!(current(root)?.change_id, before_change);
        let fresh_parent = load(root)?
            .revisions
            .into_iter()
            .find(|s| s.revision.change_id == parent.change_id)
            .unwrap()
            .revision;
        command(
            root,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.\"immutable_heads()\"",
                &parent.change_id,
            ],
        )?;
        assert!(edit(root, &fresh_parent).is_err());
        command(
            root,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.\"immutable_heads()\"",
                "root()",
            ],
        )?;
        assert!(
            edit(
                root,
                &Revision {
                    commit_id: "a".repeat(40),
                    ..fresh_parent.clone()
                }
            )
            .is_err()
        );
        command(root, &["new", "root()", "-m", "abandon target"])?;
        let abandoned = current(root)?;
        command(root, &["new", "root()", "-m", "keep current"])?;
        command(root, &["abandon", &abandoned.change_id])?;
        assert!(edit(root, &abandoned).is_err());

        let root_revision = load(root)?
            .revisions
            .into_iter()
            .find(|s| s.revision.commit_id == "0".repeat(40))
            .unwrap()
            .revision;
        assert!(edit(root, &root_revision).is_err());

        // The same Store lock used by local PR navigation blocks a viewer edit.
        let state = prepare_directory(
            root,
            &workspace_git_dir(root)?,
            &root.join("scratch/zed-pr-review"),
            None,
        )?;
        let lock = Store::at(&state.join("workspace"))?;
        assert!(
            format!("{:#}", edit(root, &fresh_parent).unwrap_err())
                .contains("Another review command")
        );
        drop(lock);
        edit(root, &fresh_parent)?;

        // Concurrent rewrites of the same change produce default /offset labels.
        command(root, &["new", "-m", "divergent target"])?;
        let divergent_target = current(root)?;
        command(root, &["new", "-m", "after divergent target"])?;
        let base_op = operation(root)?;
        command(
            root,
            &[
                "describe",
                "-r",
                &divergent_target.change_id,
                "-m",
                "first rewrite",
            ],
        )?;
        command(
            root,
            &[
                "--at-operation",
                base_op.trim(),
                "describe",
                "-r",
                &divergent_target.change_id,
                "-m",
                "second rewrite",
            ],
        )?;
        command(root, &["status"])?;
        let snapshot = load(root)?;
        let divergent: Vec<_> = snapshot
            .revisions
            .iter()
            .filter(|s| s.revision.change_id == divergent_target.change_id)
            .collect();
        assert_eq!(divergent.len(), 2);
        assert!(divergent.iter().all(|span| span.revision.divergent));
        for span in divergent {
            assert!(linked_text(&snapshot, span).contains('/'));
            assert!(edit(root, &span.revision).is_err());
        }
        Ok(())
    }

    #[test]
    fn review_badges_attach_pending_submitted_and_modified_states() {
        let rev_pending = Revision {
            change_id: "k".repeat(32),
            commit_id: "1".repeat(40),
            immutable: false,
            current: false,
            divergent: false,
        };
        let rev_approved = Revision {
            change_id: "m".repeat(32),
            commit_id: "2".repeat(40),
            immutable: false,
            current: false,
            divergent: false,
        };
        let rev_modified = Revision {
            change_id: "p".repeat(32),
            commit_id: "3".repeat(40),
            immutable: false,
            current: true,
            divergent: false,
        };
        let output = format!(
            "@  {}pppppppp{} review/pr-123\n○  {}kkkkkkkk{} review/pr-122\n○  {}mmmmmmmm{} review/pr-121\n",
            open(&rev_modified),
            CLOSE,
            open(&rev_pending),
            CLOSE,
            open(&rev_approved),
            CLOSE,
        );
        let mut snapshot = parse(output.as_bytes());
        let statuses = HashMap::from([
            (
                rev_pending.change_id.clone(),
                ReviewStatus {
                    pr_number: 122,
                    expected_commit_id: rev_pending.commit_id.clone(),
                    pending_comments: 2,
                    submitted_state: None,
                },
            ),
            (
                rev_approved.change_id.clone(),
                ReviewStatus {
                    pr_number: 121,
                    expected_commit_id: rev_approved.commit_id.clone(),
                    pending_comments: 1,
                    submitted_state: Some("APPROVED".into()),
                },
            ),
            (
                rev_modified.change_id.clone(),
                ReviewStatus {
                    pr_number: 123,
                    expected_commit_id: "f".repeat(40),
                    pending_comments: 1,
                    submitted_state: None,
                },
            ),
        ]);
        attach_review_badges(&mut snapshot, &statuses);
        let lines = text(&snapshot);
        assert!(lines[0].ends_with("[! modified]"));
        assert!(lines[1].ends_with("[2 pending]"));
        assert!(lines[2].ends_with("[✓ approved]"));
    }

    #[test]
    fn ci_badges_use_expected_symbols_and_colors() {
        let passing = format_ci_badge(CiBadge::Passing);
        assert_eq!(passing.content.as_ref(), "✓");
        assert_eq!(passing.style.fg, Some(Color::Green));

        let optional = format_ci_badge(CiBadge::OptionalFailed);
        assert_eq!(optional.content.as_ref(), "!");
        assert_eq!(optional.style.fg, Some(Color::Yellow));

        let required = format_ci_badge(CiBadge::RequiredFailed);
        assert_eq!(required.content.as_ref(), "✗");
        assert_eq!(required.style.fg, Some(Color::Red));
    }
}
