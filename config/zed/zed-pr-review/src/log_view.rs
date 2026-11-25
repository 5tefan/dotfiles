//! An interactive jj log. Refreshes preserve a semantic viewport anchor; only a
//! change of working-copy identity brings @ back into view automatically.

use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use unicode_width::UnicodeWidthStr;

use crate::ci_status::{self, CiBadge, CiRequest};
use crate::feedback::{self, ThreadBadgeRequest};
use crate::log_data::{self, Revision, RevisionSpan, Snapshot};

const REFRESH: Duration = Duration::from_secs(1);

#[derive(Default)]
struct View {
    snapshot: Option<Snapshot>,
    top: usize,
    left: usize,
    height: usize,
    width: usize,
    selected: Option<Revision>,
    reveal_when_sized: bool,
}

fn same_change(a: &Revision, b: &Revision) -> bool {
    a.change_id == b.change_id && (!(a.divergent || b.divergent) || a.commit_id == b.commit_id)
}

impl View {
    fn current(&self) -> Option<&RevisionSpan> {
        self.snapshot
            .as_ref()?
            .revisions
            .iter()
            .find(|span| span.revision.current)
    }

    fn apply(&mut self, snapshot: Snapshot) {
        let old_current = self.current().map(|span| span.revision.clone());
        let new_current = snapshot.revisions.iter().find(|span| span.revision.current);
        let moved = match (old_current.as_ref(), new_current) {
            (Some(old), Some(new)) => !same_change(old, &new.revision),
            (None, Some(_)) => true,
            _ => false,
        };
        if let Some(old) = &self.snapshot {
            // Anchor within the preceding revision, even with a long description.
            // Match repeated ID occurrences in order, rather than jumping to the
            // first occurrence. A following revision is a fallback for graph edges.
            let anchor = old
                .revisions
                .iter()
                .enumerate()
                .filter_map(|(index, before)| {
                    let occurrence = old.revisions[..index]
                        .iter()
                        .filter(|span| same_change(&before.revision, &span.revision))
                        .count();
                    snapshot
                        .revisions
                        .iter()
                        .filter(|after| same_change(&before.revision, &after.revision))
                        .nth(occurrence)
                        .map(|after| (before, after))
                })
                .min_by_key(|(before, _)| (before.row > self.top, before.row.abs_diff(self.top)));
            if let Some((before, after)) = anchor {
                self.top = after
                    .row
                    .saturating_add_signed(self.top as isize - before.row as isize);
            }
        }
        self.selected = self.selected.as_ref().and_then(|selected| {
            snapshot
                .revisions
                .iter()
                .find(|span| same_change(selected, &span.revision))
                .map(|span| span.revision.clone())
        });
        self.snapshot = Some(snapshot);
        self.clamp();
        if moved {
            self.show_current();
        }
    }

    fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.clamp();
        if self.reveal_when_sized && height > 0 {
            self.show_current();
        }
    }

    fn clamp(&mut self) {
        if let Some(snapshot) = &self.snapshot {
            self.top = self
                .top
                .min(snapshot.lines.len().saturating_sub(self.height));
            let width = snapshot.lines.iter().map(Line::width).max().unwrap_or(0);
            self.left = self.left.min(width.saturating_sub(self.width));
        }
    }

    fn scroll(&mut self, rows: isize, columns: isize) {
        self.top = self.top.saturating_add_signed(rows);
        self.left = self.left.saturating_add_signed(columns);
        self.clamp();
    }

    fn reveal(&mut self, row: usize) {
        if self.height == 0 {
            return;
        }
        if row < self.top {
            self.top = row;
        } else if row >= self.top + self.height {
            // Leave room for the description below @ when space permits.
            self.top = (row + usize::from(self.height > 1) + 1).saturating_sub(self.height);
        }
        self.clamp();
    }

    fn show_current(&mut self) {
        self.reveal_when_sized = self.height == 0;
        if let Some(span) = self.current().cloned() {
            self.reveal(span.row);
            let text: String = self.snapshot.as_ref().unwrap().lines[span.row]
                .spans
                .iter()
                .map(|part| part.content.as_ref())
                .collect();
            let node_column = text
                .find('@')
                .map(|index| text[..index].width())
                .filter(|column| *column < span.columns.start)
                .unwrap_or(span.columns.start);
            // Show both the graph node and ID if they fit; in a very narrow
            // viewport, prioritize the @ node instead of the end of the ID.
            self.left = span.columns.end.saturating_sub(self.width).min(node_column);
            self.clamp();
            self.selected = Some(span.revision);
        }
    }

    fn select(&mut self, backwards: bool) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if snapshot.revisions.is_empty() {
            return;
        }
        let mut revisions: Vec<&RevisionSpan> = Vec::new();
        for span in &snapshot.revisions {
            if !revisions
                .iter()
                .any(|previous| same_change(&previous.revision, &span.revision))
            {
                revisions.push(span);
            }
        }
        let current = self.selected.as_ref().and_then(|revision| {
            revisions
                .iter()
                .position(|s| same_change(&s.revision, revision))
        });
        let index = match current {
            Some(i) if backwards => i.saturating_sub(1),
            Some(i) => (i + 1).min(revisions.len() - 1),
            None => revisions
                .iter()
                .position(|s| s.row >= self.top)
                .unwrap_or(0),
        };
        let span = revisions[index].clone();
        self.selected = Some(span.revision);
        self.reveal(span.row);
        if span.columns.start < self.left {
            self.left = span.columns.start;
        } else if span.columns.end > self.left + self.width {
            self.left = span.columns.end.saturating_sub(self.width);
        }
        self.clamp();
    }

    fn hit(&self, row: usize, column: usize) -> Option<Revision> {
        if row >= self.height || column >= self.width {
            return None;
        }
        self.snapshot.as_ref()?.revisions.iter().find_map(|span| {
            (span.row == row + self.top && span.columns.contains(&(column + self.left)))
                .then(|| span.revision.clone())
        })
    }
}

#[derive(Default)]
struct Click {
    pressed: Option<(u16, u16, Revision)>,
}

impl Click {
    fn press(&mut self, x: u16, y: u16, target: Option<Revision>) {
        self.pressed = target.map(|revision| (x, y, revision));
    }

    fn release(&mut self, x: u16, y: u16, target: Option<Revision>) -> Option<Revision> {
        let (old_x, old_y, old) = self.pressed.take()?;
        let target = target?;
        (x == old_x && y == old_y && target == old).then_some(target)
    }
}

enum Job {
    Load,
    Edit(Revision),
}

enum Reply {
    Loaded(Result<Snapshot>),
    Edited(Result<String>),
}

// Only one job runs at a time. Slow repository reads do not block scrolling,
// redraws, or exit, and a refresh can never overlap an edit in this viewer.
fn worker(root: &Path) -> Result<(mpsc::Sender<Job>, mpsc::Receiver<Reply>)> {
    let root = root.to_owned();
    let (requests, jobs) = mpsc::channel();
    let (results, replies) = mpsc::channel();
    std::thread::Builder::new()
        .name("jj-log".into())
        .spawn(move || {
            while let Ok(job) = jobs.recv() {
                let reply = match job {
                    Job::Load => Reply::Loaded(log_data::load(&root)),
                    Job::Edit(target) => Reply::Edited(log_data::edit(&root, &target)),
                };
                if results.send(reply).is_err() {
                    break;
                }
            }
        })?;
    Ok((requests, replies))
}

pub(crate) struct Screen {
    pub(crate) stopped: Arc<AtomicBool>,
    signals: Vec<signal_hook::SigId>,
    default_signals: Arc<AtomicBool>,
}

impl Screen {
    pub(crate) fn enter() -> Result<Self> {
        // Unregistering callbacks does not restore default signal handling. Keep
        // these callbacks for the process lifetime, enabled outside a Screen.
        // Install them while enabled so partial setup cannot swallow signals.
        static DEFAULT_SIGNALS: OnceLock<io::Result<Arc<AtomicBool>>> = OnceLock::new();
        let default_signals = match DEFAULT_SIGNALS.get_or_init(|| {
            let enabled = Arc::new(AtomicBool::new(true));
            for signal in [SIGINT, SIGTERM, SIGHUP] {
                signal_hook::flag::register_conditional_default(signal, enabled.clone())?;
            }
            Ok(enabled)
        }) {
            Ok(enabled) => enabled.clone(),
            Err(error) => bail!("Cannot install default signal handlers: {error}"),
        };
        let mut screen = Self {
            stopped: Arc::new(AtomicBool::new(false)),
            signals: Vec::new(),
            default_signals,
        };
        for signal in [SIGINT, SIGTERM, SIGHUP] {
            screen
                .signals
                .push(signal_hook::flag::register(signal, screen.stopped.clone())?);
        }
        screen.default_signals.store(false, Ordering::SeqCst);
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
        self.default_signals.store(true, Ordering::SeqCst);
        for signal in self.signals.drain(..) {
            signal_hook::low_level::unregister(signal);
        }
    }
}

pub(crate) fn plain_status(value: &str) -> String {
    value
        .chars()
        .take(500)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    view: &mut View,
    ci_badges: &HashMap<String, CiBadge>,
    thread_badges: &HashMap<String, usize>,
    status: &str,
    editing: bool,
) -> Result<Rect> {
    let mut body = Rect::default();
    terminal.draw(|frame| {
        let area = frame.area();
        body = Rect::new(area.x, area.y.saturating_add(1), area.width, area.height.saturating_sub(3));
        view.resize(body.width as usize, body.height as usize);
        let count = view.snapshot.as_ref().map_or(0, |s| s.lines.len());
        let title = format!("jj l · {} · lines {}–{}/{}", if editing { "editing…" } else { "live" },
            if count == 0 { 0 } else { view.top + 1 }, (view.top + view.height).min(count), count);
        frame.render_widget(Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
            Rect::new(area.x, area.y, area.width, area.height.min(1)));
        if let Some(snapshot) = &view.snapshot {
            let mut lines: Vec<_> = snapshot.lines.iter().skip(view.top).take(view.height).cloned().collect();
            if !ci_badges.is_empty() || !thread_badges.is_empty() {
                let mut badged_rows = HashSet::new();
                for span in &snapshot.revisions {
                    if span.row < view.top || span.row >= view.top + view.height {
                        continue;
                    }
                    if !badged_rows.insert(span.row) {
                        continue;
                    }
                    if let Some(line) = lines.get_mut(span.row - view.top) {
                        if let Some(&badge) = ci_badges.get(&span.revision.commit_id) {
                            line.spans.push(ratatui::text::Span::raw(" "));
                            line.spans.push(log_data::format_ci_badge(badge));
                        }
                        if let Some(&count) = thread_badges.get(&span.revision.commit_id)
                            && count > 0
                        {
                            line.spans.push(ratatui::text::Span::raw(" "));
                            line.spans.push(log_data::format_thread_badge(count));
                        }
                    }
                }
            }
            frame.render_widget(Paragraph::new(Text::from(lines)).scroll((0, view.left.min(u16::MAX as usize) as u16)), body);
            for span in &snapshot.revisions {
                if span.row < view.top || span.row >= view.top + view.height {
                    continue;
                }
                let selected = view.selected.as_ref().is_some_and(|r| same_change(r, &span.revision));
                let mut style = Style::default();
                if !span.revision.immutable && !span.revision.divergent {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                for column in span.columns.start.max(view.left)..span.columns.end.min(view.left + view.width) {
                    if let Some(cell) = frame.buffer_mut().cell_mut((body.x + (column - view.left) as u16,
                        body.y + (span.row - view.top) as u16)) {
                        cell.set_style(style);
                    }
                }
            }
        }
        if area.height >= 2 {
            frame.render_widget(Paragraph::new(plain_status(status)),
                Rect::new(area.x, area.y + area.height - 2, area.width, 1));
        }
        if area.height >= 3 {
            frame.render_widget(Paragraph::new("Click ID: edit · j/k/wheel: scroll · Tab/Enter: select/edit · @: current · r: refresh · q: quit")
                .style(Style::default().fg(Color::DarkGray)),
                Rect::new(area.x, area.y + area.height - 1, area.width, 1));
        }
    })?;
    Ok(body)
}

/// Requires a terminal on stdin and stdout. The caller retains one-shot output
/// for pipes, rather than trying to start a mouse interface without a terminal.
pub fn run(root: &Path) -> Result<()> {
    let screen = Screen::enter().context("Cannot enter the interactive jj log")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let (requests, replies) = worker(root)?;
    let mut ci_worker = ci_status::spawn_worker(root);
    let mut ci_badges: HashMap<String, CiBadge> = HashMap::new();
    let mut thread_worker = feedback::spawn_badge_worker(root);
    let mut thread_badges: HashMap<String, usize> = HashMap::new();
    let mut view = View::default();
    let mut click = Click::default();
    let mut pending = None;
    let mut busy = false;
    let mut editing = false;
    let mut quit = false;
    let mut next_refresh = Instant::now();
    let mut status = String::from("Loading jj l…");
    let mut read_failed = false;
    let mut dirty = true;
    let mut body = Rect::default();
    loop {
        if let Some(ci) = ci_worker.as_ref() {
            let mut disconnected = false;
            loop {
                match ci.replies.try_recv() {
                    Ok(badges) => {
                        if badges != ci_badges {
                            ci_badges = badges;
                            dirty = true;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                ci_worker = None;
            }
        }
        if let Some(tb) = thread_worker.as_ref() {
            let mut disconnected = false;
            loop {
                match tb.replies.try_recv() {
                    Ok(badges) => {
                        if badges != thread_badges {
                            thread_badges = badges;
                            dirty = true;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                thread_worker = None;
            }
        }
        let reply = match replies.try_recv() {
            Ok(Reply::Edited(result)) => {
                busy = false;
                editing = false;
                status = match result {
                    Ok(message) => message,
                    Err(error) => format!("{error:#}"),
                };
                // Report mutation completion before starting the next read, so
                // even a slow post-edit refresh cannot hold the terminal open.
                next_refresh = Instant::now();
                dirty = true;
                None
            }
            Ok(Reply::Loaded(result)) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("The jj log worker stopped unexpectedly")
            }
        };
        if let Some(loaded) = reply {
            busy = false;
            match loaded {
                Ok(snapshot) => {
                    let linked = !snapshot.revisions.is_empty();
                    if ci_worker.is_some() || thread_worker.is_some() {
                        let commits: Vec<String> = snapshot
                            .revisions
                            .iter()
                            .map(|s| s.revision.commit_id.clone())
                            .collect();
                        if let Some(ci) = ci_worker.as_ref() {
                            let _ = ci.requests.send(CiRequest::UpdateCommits(commits.clone()));
                        }
                        if let Some(tb) = thread_worker.as_ref() {
                            let _ = tb.requests.send(ThreadBadgeRequest::UpdateCommits(commits));
                        }
                    }
                    view.apply(snapshot);
                    if read_failed || status == "Loading jj l…" {
                        status = if linked { "Mutable IDs are underlined. Shift-drag selects text." }
                            else { "This jj template has no linked IDs; use the standard change-ID formatter for clicks and @ tracking." }.into();
                    }
                    read_failed = false;
                }
                Err(error) => {
                    status = format!("Refresh failed: {error:#}");
                    read_failed = true;
                    // Keep the viewport, but never act on a failed/stale refresh.
                    click.pressed = None;
                    pending = None;
                }
            }
            next_refresh = Instant::now() + REFRESH;
            dirty = true;
        }
        if screen.stopped.load(Ordering::Relaxed) {
            quit = true;
        }
        if quit && !editing {
            break;
        }
        if !busy {
            if let Some(target) = pending.take() {
                requests.send(Job::Edit(target))?;
                busy = true;
                editing = true;
            } else if Instant::now() >= next_refresh && !quit {
                requests.send(Job::Load)?;
                busy = true;
            }
        }
        if dirty {
            body = draw(
                &mut terminal,
                &mut view,
                &ci_badges,
                &thread_badges,
                &status,
                editing || pending.is_some(),
            )?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let event = event::read()?;
        let mut target = None;
        match event {
            Event::Resize(_, _) => {
                click.pressed = None;
                dirty = true;
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                dirty = true;
                click.pressed = None;
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => quit = true,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        quit = true
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        view.scroll(-(view.height as isize / 2).max(1), 0)
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        view.scroll((view.height as isize / 2).max(1), 0)
                    }
                    KeyCode::Up | KeyCode::Char('k') => view.scroll(-1, 0),
                    KeyCode::Down | KeyCode::Char('j') => view.scroll(1, 0),
                    KeyCode::Left | KeyCode::Char('h') => view.scroll(0, -4),
                    KeyCode::Right | KeyCode::Char('l') => view.scroll(0, 4),
                    KeyCode::PageUp => view.scroll(-(view.height as isize).max(1), 0),
                    KeyCode::PageDown => view.scroll((view.height as isize).max(1), 0),
                    KeyCode::Home | KeyCode::Char('g') => view.top = 0,
                    KeyCode::End | KeyCode::Char('G') => {
                        view.top = usize::MAX;
                        view.clamp();
                    }
                    KeyCode::Tab => view.select(key.modifiers.contains(KeyModifiers::SHIFT)),
                    KeyCode::BackTab => view.select(true),
                    KeyCode::Char('@') => view.show_current(),
                    KeyCode::Char('r') => next_refresh = Instant::now(),
                    KeyCode::Enter if key.kind == KeyEventKind::Press => {
                        target = view.selected.clone()
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                let inside = body.contains((mouse.column, mouse.row).into());
                let hit = if inside {
                    view.hit(
                        (mouse.row - body.y) as usize,
                        (mouse.column - body.x) as usize,
                    )
                } else {
                    None
                };
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) if !editing && pending.is_none() => {
                        click.press(mouse.column, mouse.row, hit)
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        target = click.release(mouse.column, mouse.row, hit)
                    }
                    MouseEventKind::Drag(_) => click.pressed = None,
                    MouseEventKind::ScrollUp if inside => {
                        view.scroll(-3, 0);
                        click.pressed = None;
                        dirty = true;
                    }
                    MouseEventKind::ScrollDown if inside => {
                        view.scroll(3, 0);
                        click.pressed = None;
                        dirty = true;
                    }
                    MouseEventKind::ScrollLeft if inside => {
                        view.scroll(0, -4);
                        click.pressed = None;
                        dirty = true;
                    }
                    MouseEventKind::ScrollRight if inside => {
                        view.scroll(0, 4);
                        click.pressed = None;
                        dirty = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if let Some(target) = target.filter(|_| !editing && pending.is_none() && !quit) {
            view.selected = Some(target.clone());
            dirty = true;
            status = if read_failed {
                "Refresh failed; wait for a successful refresh before editing.".into()
            } else if target.current {
                "Already editing this revision.".into()
            } else if target.immutable {
                "Immutable revision; no checkout was attempted.".into()
            } else if target.divergent {
                "Divergent change; resolve its divergence before editing.".into()
            } else {
                pending = Some(target);
                "Editing…".into()
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(ids: &[usize], current: usize) -> Snapshot {
        Snapshot {
            lines: ids
                .iter()
                .flat_map(|id| {
                    [
                        Line::from(format!("o change {id}")),
                        Line::from("│ description"),
                    ]
                })
                .collect(),
            revisions: ids
                .iter()
                .enumerate()
                .map(|(index, id)| RevisionSpan {
                    row: index * 2,
                    columns: 2..10,
                    revision: Revision {
                        change_id: id.to_string(),
                        commit_id: format!("commit-{id}"),
                        immutable: false,
                        current: *id == current,
                        divergent: false,
                    },
                })
                .collect(),
        }
    }

    fn view() -> View {
        let mut view = View::default();
        view.resize(80, 6);
        view
    }

    #[test]
    fn starts_with_current_visible_even_below_first_page() {
        let mut v = view();
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        assert!(v.top > 0);
        assert!((v.top..v.top + v.height).contains(&60));
    }

    #[test]
    fn periodic_refresh_does_not_pull_user_back_to_current() {
        let mut v = view();
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        v.scroll(-100, 0);
        for _ in 0..3 {
            v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
            assert_eq!(v.top, 0);
        }
    }

    #[test]
    fn content_snapshot_of_current_does_not_scroll() {
        let mut v = view();
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        v.top = 3;
        let mut next = snapshot(&(0..40).collect::<Vec<_>>(), 30);
        next.revisions[30].revision.commit_id = "rewritten".into();
        v.apply(next);
        assert_eq!(v.top, 3);
    }

    #[test]
    fn changing_current_scrolls_into_view_then_leaves_manual_scroll_alone() {
        let mut v = view();
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 0));
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        assert!((v.top..v.top + v.height).contains(&60));
        v.top = 0;
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        assert_eq!(v.top, 0);
        v.top = 60;
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 1));
        assert!((v.top..v.top + v.height).contains(&2));
    }

    #[test]
    fn preserves_revision_and_intra_revision_offset_when_rows_inserted_or_removed() {
        let mut v = view();
        v.apply(snapshot(&(10..30).collect::<Vec<_>>(), 10));
        v.top = 11; // description of change 15
        v.apply(snapshot(&(0..30).collect::<Vec<_>>(), 10));
        assert_eq!(v.top, 31);
        v.apply(snapshot(&(13..30).collect::<Vec<_>>(), 10));
        assert_eq!(v.top, 5);
    }

    #[test]
    fn removed_anchor_uses_nearby_surviving_revision() {
        let mut v = view();
        v.apply(snapshot(&(0..20).collect::<Vec<_>>(), 0));
        v.top = 10;
        v.apply(snapshot(&[0, 1, 2, 3, 4, 6, 7, 8, 9, 10, 11, 12], 0));
        assert_eq!(v.top, 10);
        assert_eq!(v.hit(0, 2).unwrap().change_id, "6");
    }

    #[test]
    fn resize_clamps_without_recentering_current() {
        let mut v = view();
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        v.top = 5;
        v.resize(40, 10);
        assert_eq!(v.top, 5);
        v.top = 76;
        v.resize(40, 20);
        assert_eq!(v.top, 60);
    }

    #[test]
    fn hit_testing_uses_both_scroll_offsets_and_id_columns() {
        let mut v = view();
        v.apply(snapshot(&(0..20).collect::<Vec<_>>(), 0));
        v.top = 4;
        v.left = 4;
        assert_eq!(v.hit(0, 0).unwrap().change_id, "2");
        assert!(v.hit(0, 6).is_none());
        assert!(v.hit(1, 0).is_none());
        assert!(v.hit(v.height, 0).is_none());
    }

    #[test]
    fn click_requires_press_and_release_on_same_unchanged_id() {
        let target = snapshot(&[1], 1).revisions[0].revision.clone();
        let mut click = Click::default();
        assert!(click.release(3, 4, Some(target.clone())).is_none());
        click.press(3, 4, Some(target.clone()));
        assert!(click.release(4, 4, Some(target.clone())).is_none());
        click.press(3, 4, Some(target.clone()));
        let mut changed = target.clone();
        changed.commit_id = "new-content".into();
        assert!(click.release(3, 4, Some(changed)).is_none());
        click.press(3, 4, Some(target.clone()));
        assert_eq!(click.release(3, 4, Some(target.clone())), Some(target));
    }

    #[test]
    fn initial_current_is_revealed_when_a_collapsed_pane_becomes_visible() {
        let mut v = View::default();
        v.apply(snapshot(&(0..40).collect::<Vec<_>>(), 30));
        v.resize(80, 6);
        assert!((v.top..v.top + v.height).contains(&60));
        v.top = 0;
        v.resize(80, 8);
        assert_eq!(v.top, 0);
    }

    #[test]
    fn description_growth_below_viewport_top_does_not_move_it() {
        let mut v = view();
        let mut before = snapshot(&[0, 1, 2], 0);
        before
            .lines
            .splice(1..1, vec![Line::from("description"); 6]);
        before.revisions[1].row += 6;
        before.revisions[2].row += 6;
        v.apply(before.clone());
        v.height = 3;
        v.top = 6;
        let mut after = before;
        after
            .lines
            .splice(7..7, vec![Line::from("extra description"); 2]);
        after.revisions[1].row += 2;
        after.revisions[2].row += 2;
        v.apply(after);
        assert_eq!(v.top, 6);
    }

    #[test]
    fn duplicate_ids_preserve_occurrence_and_do_not_trap_tab() {
        let mut v = view();
        v.apply(snapshot(&[0, 1, 1, 2, 3, 4, 5], 0));
        v.top = 4;
        v.apply(snapshot(&[0, 1, 1, 2, 3, 4, 5], 0));
        assert_eq!(v.top, 4);
        v.select(false);
        assert_eq!(v.selected.as_ref().unwrap().change_id, "1");
        v.select(false);
        assert_eq!(v.selected.as_ref().unwrap().change_id, "2");
    }

    #[test]
    fn current_is_revealed_horizontally_in_a_wide_graph() {
        let mut v = view();
        v.width = 12;
        let mut next = snapshot(&[0], 0);
        next.lines[0] = Line::from(format!("{}@  current0", "│ ".repeat(50)));
        next.revisions[0].columns = 103..111;
        v.apply(next);
        assert!((v.left..v.left + v.width).contains(&100));
        assert!(v.left <= 103 && v.left + v.width >= 111);
        v.width = 2;
        v.show_current();
        assert!((v.left..v.left + v.width).contains(&100));
    }

    #[test]
    fn empty_and_tiny_viewports_are_safe() {
        let mut v = View::default();
        v.apply(snapshot(&[], 0));
        v.scroll(10, 10);
        v.select(false);
        v.show_current();
        assert!(v.hit(0, 0).is_none());
        v.resize(1, 1);
        v.apply(snapshot(&[0, 1, 2], 2));
        assert_eq!(v.top, 4);
    }
}
