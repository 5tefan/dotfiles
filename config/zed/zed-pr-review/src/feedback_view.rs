//! Interactive TUI (`ctrl-shift-f`) for viewing and acting on PR feedback.
//!
//! Dual-purpose:
//! - On your own stack: shows GitHub review threads grouped by revision/bookmark
//!   with local reply drafting and local resolve toggling.
//! - On a `review/pr-*` review stack: shows your staged inline review comments
//!   alongside existing GitHub review threads on that PR.

use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::feedback::{self, FeedbackDraft, FeedbackIndex, IndexedThread};
use crate::github::Client;
use crate::log_view::{Screen, plain_status};
use crate::message::review_summary;
use crate::model::{Comment, PullRequest};
use crate::store::{Draft, Store};

const SNIPPET_CONTEXT: usize = 4;

#[derive(Clone, Debug)]
pub struct ReviewContext {
    pub pr: PullRequest,
    pub draft_dir: PathBuf,
}

#[derive(Clone, Debug)]
enum ListRow {
    Header(String),
    ReviewComment {
        comment_index: usize,
        comment: Comment,
    },
    Thread(Box<IndexedThread>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusTarget {
    Snippet,
    PrimaryEdit, // Reply (for thread) or Edit (for review comment)
    Resolve,     // Resolve/Unresolve (for thread)
    RemoveOrClear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingAction {
    OpenCode {
        checkout: bool,
        line_override: Option<u32>,
    },
    EditOrReply,
    ToggleResolve,
    ClearOrRemove,
    Refresh,
}

#[derive(Clone, Debug)]
struct HitRegion {
    rect: Rect,
    action: HitAction,
}

#[derive(Clone, Copy, Debug)]
enum HitAction {
    SelectRow(usize),
    ToggleRowResolve(usize),
    OpenSnippetLine(u32),
    Trigger(PendingAction),
}

struct SnippetLine {
    line_no: u32,
    highlighted: bool,
    spans: Vec<Span<'static>>,
}

struct AppState {
    rows: Vec<ListRow>,
    selected_row: Option<usize>,
    list_scroll: usize,
    focus: FocusTarget,
    include_all: bool,
    status: String,
    index: FeedbackIndex,
    drafts: HashMap<(u64, String), FeedbackDraft>,
    review_comments: Vec<Comment>,
    review_pr: Option<PullRequest>,
}

impl AppState {
    fn new(
        index: FeedbackIndex,
        drafts: HashMap<(u64, String), FeedbackDraft>,
        review_pr: Option<PullRequest>,
        review_comments: Vec<Comment>,
        include_all: bool,
    ) -> Self {
        let mut state = Self {
            rows: Vec::new(),
            selected_row: None,
            list_scroll: 0,
            focus: FocusTarget::Snippet,
            include_all,
            status:
                "j/k: navigate · Tab/h/l: focus · Space: trigger · click snippet/buttons · q: quit"
                    .into(),
            index,
            drafts,
            review_comments,
            review_pr,
        };
        state.rebuild_rows(None);
        state
    }

    fn rebuild_rows(&mut self, preserve_thread_id: Option<&str>) {
        let mut rows = Vec::new();
        if let Some(ref pr) = self.review_pr {
            rows.push(ListRow::Header(format!(
                "Your Staged Comments — PR #{} ({})",
                pr.number,
                self.review_comments.len()
            )));
            for (idx, comment) in self.review_comments.iter().enumerate() {
                rows.push(ListRow::ReviewComment {
                    comment_index: idx,
                    comment: comment.clone(),
                });
            }
        }

        let visible = feedback::visible_threads(&self.index, self.include_all);
        let mut current_group: Option<usize> = None;
        for item in visible {
            if current_group != Some(item.group_index) {
                current_group = Some(item.group_index);
                let title = if self.review_pr.is_some() {
                    format!("Existing PR #{} Review Threads", item.pr_number)
                } else {
                    format!(
                        "{} — PR #{} {}",
                        item.bookmark,
                        item.pr_number,
                        plain_status(&item.pr_title)
                    )
                };
                rows.push(ListRow::Header(title));
            }
            rows.push(ListRow::Thread(Box::new(item)));
        }

        self.rows = rows;
        let next_sel = if let Some(tid) = preserve_thread_id {
            self.rows
                .iter()
                .position(|r| matches!(r, ListRow::Thread(t) if t.thread.id == tid))
        } else {
            None
        };
        self.selected_row = next_sel.or_else(|| {
            self.selected_row
                .filter(|&i| i < self.rows.len() && !matches!(self.rows[i], ListRow::Header(_)))
                .or_else(|| {
                    self.rows
                        .iter()
                        .position(|r| !matches!(r, ListRow::Header(_)))
                })
        });
        self.clamp_focus();
    }

    fn selected(&self) -> Option<&ListRow> {
        self.selected_row.and_then(|i| self.rows.get(i))
    }

    fn available_focus_targets(&self) -> Vec<FocusTarget> {
        match self.selected() {
            Some(ListRow::ReviewComment { .. }) => vec![
                FocusTarget::Snippet,
                FocusTarget::PrimaryEdit,
                FocusTarget::RemoveOrClear,
            ],
            Some(ListRow::Thread(item)) => {
                let mut targets = vec![
                    FocusTarget::Snippet,
                    FocusTarget::PrimaryEdit,
                    FocusTarget::Resolve,
                ];
                if self
                    .drafts
                    .get(&(item.pr_number, item.thread.id.clone()))
                    .is_some_and(FeedbackDraft::has_staged_work)
                {
                    targets.push(FocusTarget::RemoveOrClear);
                }
                targets
            }
            _ => vec![FocusTarget::Snippet],
        }
    }

    fn clamp_focus(&mut self) {
        let targets = self.available_focus_targets();
        if !targets.contains(&self.focus) {
            self.focus = targets[0];
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let selectable: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| (!matches!(r, ListRow::Header(_))).then_some(i))
            .collect();
        if selectable.is_empty() {
            self.selected_row = None;
            return;
        }
        let current_pos = self
            .selected_row
            .and_then(|sel| selectable.iter().position(|&i| i == sel))
            .unwrap_or(0);
        let next_pos = if delta >= 0 {
            (current_pos + delta as usize).min(selectable.len() - 1)
        } else {
            current_pos.saturating_sub(delta.unsigned_abs())
        };
        self.selected_row = Some(selectable[next_pos]);
        self.clamp_focus();
    }

    fn cycle_focus(&mut self, backward: bool) {
        let targets = self.available_focus_targets();
        if targets.is_empty() {
            return;
        }
        let pos = targets.iter().position(|&t| t == self.focus).unwrap_or(0);
        let next = if backward {
            (pos + targets.len() - 1) % targets.len()
        } else {
            (pos + 1) % targets.len()
        };
        self.focus = targets[next];
    }

    fn focus_action(&self) -> Option<PendingAction> {
        self.selected()?;
        match self.focus {
            FocusTarget::Snippet => Some(PendingAction::OpenCode {
                checkout: false,
                line_override: None,
            }),
            FocusTarget::PrimaryEdit => Some(PendingAction::EditOrReply),
            FocusTarget::Resolve => Some(PendingAction::ToggleResolve),
            FocusTarget::RemoveOrClear => Some(PendingAction::ClearOrRemove),
        }
    }
}

pub fn run(
    client: &Client,
    feedback_dir: &Path,
    initial_index: FeedbackIndex,
    review_ctx: Option<ReviewContext>,
    explicit_pr: Option<&str>,
    revset: Option<&str>,
    include_all: bool,
    mut edit_fn: impl FnMut(&Store, &str, &str) -> Result<String>,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print_plain_summary(&initial_index, feedback_dir, include_all);
        return Ok(());
    }

    let (review_pr, review_comments) = match &review_ctx {
        Some(ctx) => {
            let comments = load_review_comments(&ctx.draft_dir).unwrap_or_default();
            (Some(ctx.pr.clone()), comments)
        }
        None => (None, Vec::new()),
    };
    let drafts = feedback::load_draft_map(feedback_dir);
    let mut app = AppState::new(
        initial_index,
        drafts,
        review_pr,
        review_comments,
        include_all,
    );

    let mut screen = Some(Screen::enter().context("Cannot enter the feedback viewer")?);
    let mut terminal = Some(Terminal::new(CrosstermBackend::new(io::stdout()))?);
    if let Some(term) = terminal.as_mut() {
        term.clear()?;
    }

    let mut hit_regions: Vec<HitRegion> = Vec::new();
    let mut dirty = true;

    loop {
        if screen
            .as_ref()
            .is_some_and(|s| s.stopped.load(Ordering::Relaxed))
        {
            break;
        }

        if dirty && let Some(term) = terminal.as_mut() {
            hit_regions.clear();
            term.draw(|frame| {
                draw(frame, client, &mut app, &mut hit_regions);
            })?;
            dirty = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        let mut pending: Option<PendingAction> = None;
        match event::read()? {
            Event::Resize(_, _) => dirty = true,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if handle_key(&mut app, key, &mut pending) {
                    break;
                }
                dirty = true;
            }
            Event::Mouse(mouse) => {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    handle_click(
                        &mut app,
                        mouse.column,
                        mouse.row,
                        &hit_regions,
                        &mut pending,
                    );
                    dirty = true;
                } else if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                    app.move_selection(1);
                    dirty = true;
                } else if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    app.move_selection(-1);
                    dirty = true;
                }
            }
            _ => {}
        }

        if let Some(action) = pending {
            let needs_terminal_suspend = matches!(action, PendingAction::EditOrReply);
            if needs_terminal_suspend {
                drop(terminal.take());
                drop(screen.take());
            }

            let outcome = execute_action(
                client,
                feedback_dir,
                &mut app,
                review_ctx.as_ref(),
                explicit_pr,
                revset,
                action,
                &mut edit_fn,
            );
            match outcome {
                Ok(msg) => app.status = msg,
                Err(err) => app.status = format!("Error: {err:#}"),
            }

            if needs_terminal_suspend {
                screen = Some(Screen::enter().context("Cannot re-enter the feedback viewer")?);
                let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                term.clear()?;
                terminal = Some(term);
            }
            dirty = true;
        }
    }

    drop(terminal);
    drop(screen);
    Ok(())
}

fn handle_key(app: &mut AppState, key: KeyEvent, pending: &mut Option<PendingAction>) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => app.cycle_focus(false),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => app.cycle_focus(true),
        KeyCode::Char(' ') | KeyCode::Enter if key.kind == KeyEventKind::Press => {
            *pending = app.focus_action();
        }
        KeyCode::Char('o') if key.kind == KeyEventKind::Press => {
            *pending = Some(PendingAction::OpenCode {
                checkout: false,
                line_override: None,
            });
        }
        KeyCode::Char('O') if key.kind == KeyEventKind::Press => {
            *pending = Some(PendingAction::OpenCode {
                checkout: true,
                line_override: None,
            });
        }
        KeyCode::Char('r') | KeyCode::Char('e') if key.kind == KeyEventKind::Press => {
            *pending = Some(PendingAction::EditOrReply);
        }
        KeyCode::Char('x') if key.kind == KeyEventKind::Press => {
            *pending = Some(PendingAction::ToggleResolve);
        }
        KeyCode::Char('c') | KeyCode::Char('d') if key.kind == KeyEventKind::Press => {
            *pending = Some(PendingAction::ClearOrRemove);
        }
        KeyCode::Char('a') if key.kind == KeyEventKind::Press => {
            app.include_all = !app.include_all;
            let keep = match app.selected() {
                Some(ListRow::Thread(t)) => Some(t.thread.id.clone()),
                _ => None,
            };
            app.rebuild_rows(keep.as_deref());
            app.status = if app.include_all {
                "Showing all threads (including resolved)".into()
            } else {
                "Showing unresolved threads".into()
            };
        }
        KeyCode::Char('f') if key.kind == KeyEventKind::Press => {
            *pending = Some(PendingAction::Refresh);
        }
        _ => {}
    }
    false
}

fn handle_click(
    app: &mut AppState,
    col: u16,
    row: u16,
    regions: &[HitRegion],
    pending: &mut Option<PendingAction>,
) {
    // Check in reverse order so more-specific regions (like inline [Resolve] button) win over row selection.
    for region in regions.iter().rev() {
        if col >= region.rect.x
            && col < region.rect.x.saturating_add(region.rect.width)
            && row >= region.rect.y
            && row < region.rect.y.saturating_add(region.rect.height)
        {
            match region.action {
                HitAction::SelectRow(idx) => {
                    app.selected_row = Some(idx);
                    app.clamp_focus();
                }
                HitAction::ToggleRowResolve(idx) => {
                    app.selected_row = Some(idx);
                    app.clamp_focus();
                    *pending = Some(PendingAction::ToggleResolve);
                }
                HitAction::OpenSnippetLine(line) => {
                    app.focus = FocusTarget::Snippet;
                    *pending = Some(PendingAction::OpenCode {
                        checkout: false,
                        line_override: Some(line),
                    });
                }
                HitAction::Trigger(action) => {
                    *pending = Some(action);
                }
            }
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_action(
    client: &Client,
    feedback_dir: &Path,
    app: &mut AppState,
    review_ctx: Option<&ReviewContext>,
    explicit_pr: Option<&str>,
    revset: Option<&str>,
    action: PendingAction,
    edit_fn: &mut impl FnMut(&Store, &str, &str) -> Result<String>,
) -> Result<String> {
    match action {
        PendingAction::Refresh => {
            let refreshed = if let Some(ctx) = review_ctx {
                let threads = client.fetch_review_threads(
                    ctx.pr.number,
                    &ctx.pr.head_ref,
                    &ctx.pr.head_sha,
                )?;
                FeedbackIndex {
                    version: 1,
                    host: ctx.pr.host.clone(),
                    repo: ctx.pr.repo.clone(),
                    groups: vec![feedback::IndexedGroup {
                        bookmark: ctx.pr.head_ref.clone(),
                        change_id: String::new(),
                        commit_id: String::new(),
                        pr_number: ctx.pr.number,
                        pr_title: ctx.pr.title.clone(),
                        pr_url: ctx.pr.url.clone(),
                        head_sha: ctx.pr.head_sha.clone(),
                        threads,
                    }],
                }
            } else {
                feedback::fetch_stack_feedback(client, explicit_pr, revset)?
            };
            feedback::save_index(feedback_dir, &refreshed)?;
            app.index = refreshed;
            app.drafts = feedback::load_draft_map(feedback_dir);
            if let Some(ctx) = review_ctx {
                app.review_comments = load_review_comments(&ctx.draft_dir).unwrap_or_default();
            }
            app.rebuild_rows(None);
            Ok("Refreshed from GitHub.".into())
        }
        PendingAction::OpenCode {
            checkout,
            line_override,
        } => match app.selected().cloned() {
            Some(ListRow::Thread(item)) => {
                let line = line_override.or_else(|| item.thread.effective_line());
                let change_id = (!item.change_id.is_empty()).then_some(item.change_id.as_str());
                let warn = feedback::open_in_zed(
                    &client.root,
                    change_id,
                    &item.thread.path,
                    line,
                    checkout,
                )?;
                let loc = match line {
                    Some(l) => format!("{}:{l}", item.thread.path),
                    None => item.thread.path.clone(),
                };
                Ok(match warn {
                    Some(w) => format!("Opened {loc} in Zed. {w}"),
                    None => format!("Opened {loc} in Zed."),
                })
            }
            Some(ListRow::ReviewComment { comment, .. }) => {
                let line = line_override.or(Some(comment.line));
                let _ = feedback::open_in_zed(&client.root, None, &comment.path, line, false)?;
                Ok(format!(
                    "Opened {}:{} in Zed.",
                    comment.path,
                    line.unwrap_or(comment.line)
                ))
            }
            _ => Ok("Nothing selected.".into()),
        },
        PendingAction::EditOrReply => match app.selected().cloned() {
            Some(ListRow::Thread(item)) => {
                let existing_draft = app.drafts.get(&(item.pr_number, item.thread.id.clone()));
                let existing = existing_draft.and_then(|d| d.reply_body.as_deref());
                let existing_agent = existing_draft.is_some_and(|d| d.reply_agent);
                let template = feedback::reply_template(&item.thread, existing);
                let store = feedback::open_thread_store(
                    feedback_dir,
                    &app.index.host,
                    &app.index.repo,
                    item.pr_number,
                    &item.thread.id,
                )?;
                let edited = edit_fn(&store, &template, "reply")?;
                drop(store);
                let cleaned = review_summary(&edited);
                if cleaned.trim().is_empty() {
                    return Ok("Blank reply: cancelled.".into());
                }
                let draft = feedback::stage_reply(
                    feedback_dir,
                    &app.index.host,
                    &app.index.repo,
                    &item.thread,
                    &cleaned,
                    existing_agent,
                )?;
                app.drafts
                    .insert((item.pr_number, item.thread.id.clone()), draft);
                app.clamp_focus();
                Ok(format!(
                    "Saved local reply for PR #{} ({}). Publish via Ctrl-Shift-X.",
                    item.pr_number, item.thread.path
                ))
            }
            Some(ListRow::ReviewComment {
                comment_index,
                comment,
            }) => {
                let Some(ctx) = review_ctx else {
                    return Ok("Not in review mode.".into());
                };
                let store = Store::at(&ctx.draft_dir)?;
                let mut draft = store.load()?.context("Review draft missing")?;
                draft.editable()?;
                let edited = edit_fn(&store, &comment.body, "comment")?;
                if edited.trim().is_empty() {
                    return Ok("Blank comment: edit cancelled.".into());
                }
                if let Some(slot) = draft.comments.get_mut(comment_index) {
                    slot.body = edited.trim().to_owned();
                }
                store.save(&draft)?;
                app.review_comments = draft.comments;
                app.rebuild_rows(None);
                Ok(format!("Updated staged comment #{}.", comment_index + 1))
            }
            _ => Ok("Nothing selected.".into()),
        },
        PendingAction::ToggleResolve => match app.selected().cloned() {
            Some(ListRow::Thread(item)) => {
                let current = app
                    .drafts
                    .get(&(item.pr_number, item.thread.id.clone()))
                    .is_some_and(|d| d.resolve);
                let next = !current;
                let updated = feedback::set_resolve(
                    feedback_dir,
                    &app.index.host,
                    &app.index.repo,
                    &item.thread,
                    next,
                )?;
                match updated {
                    Some(d) => {
                        app.drafts
                            .insert((item.pr_number, item.thread.id.clone()), d);
                    }
                    None => {
                        app.drafts.remove(&(item.pr_number, item.thread.id.clone()));
                    }
                }
                app.clamp_focus();
                Ok(if next {
                    "Marked thread for local resolve (not yet published).".into()
                } else {
                    "Cleared local resolve flag.".into()
                })
            }
            _ => Ok("Resolve applies to GitHub review threads.".into()),
        },
        PendingAction::ClearOrRemove => match app.selected().cloned() {
            Some(ListRow::Thread(item)) => {
                let cleared = feedback::clear_staged(
                    feedback_dir,
                    &app.index.host,
                    &app.index.repo,
                    &item.thread,
                )?;
                app.drafts.remove(&(item.pr_number, item.thread.id.clone()));
                app.clamp_focus();
                Ok(if cleared {
                    "Cleared staged reply and resolve flag.".into()
                } else {
                    "No staged changes on this thread.".into()
                })
            }
            Some(ListRow::ReviewComment { comment_index, .. }) => {
                let Some(ctx) = review_ctx else {
                    return Ok("Not in review mode.".into());
                };
                let store = Store::at(&ctx.draft_dir)?;
                let mut draft = store.load()?.context("Review draft missing")?;
                draft.editable()?;
                if comment_index < draft.comments.len() {
                    draft.comments.remove(comment_index);
                    store.save(&draft)?;
                    app.review_comments = draft.comments;
                    app.rebuild_rows(None);
                    Ok(format!("Removed staged comment #{}.", comment_index + 1))
                } else {
                    Ok("Comment index out of range.".into())
                }
            }
            _ => Ok("Nothing selected.".into()),
        },
    }
}

fn load_review_comments(draft_dir: &Path) -> Result<Vec<Comment>> {
    let path = draft_dir.join("draft.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    let draft: Draft = serde_json::from_slice(&bytes)?;
    Ok(draft.comments)
}

fn draw(frame: &mut Frame, client: &Client, app: &mut AppState, hit_regions: &mut Vec<HitRegion>) {
    let area = frame.area();
    if area.height < 4 || area.width < 16 {
        return;
    }

    let [title_area, body_area, status_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let staged_replies = app
        .drafts
        .values()
        .filter(|d| {
            d.reply_body
                .as_deref()
                .is_some_and(|b| !b.trim().is_empty())
        })
        .count();
    let staged_resolves = app.drafts.values().filter(|d| d.resolve).count();
    let mode = if app.review_pr.is_some() {
        "PR Review Feedback"
    } else {
        "Stack Feedback"
    };
    let title = format!(
        " {mode} · {} staged repl{}, {} resolve{} · Ctrl-Shift-X to publish ",
        staged_replies,
        if staged_replies == 1 { "y" } else { "ies" },
        staged_resolves,
        if staged_resolves == 1 { "" } else { "s" },
    );
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        title_area,
    );

    let [list_area, detail_area] = if body_area.width >= 90 {
        Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
            .areas(body_area)
    } else {
        Layout::vertical([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body_area)
    };

    draw_list(frame, app, list_area, hit_regions);
    draw_detail(frame, client, app, detail_area, hit_regions);

    frame.render_widget(Paragraph::new(plain_status(&app.status)), status_area);
    frame.render_widget(
        Paragraph::new(
            "j/k: list · Tab/h/l: focus · Space: trigger · o/O: open(/checkout) · r: reply/edit · x: resolve · a: all · f: refresh · q: quit",
        )
        .style(Style::default().fg(Color::DarkGray)),
        help_area,
    );
}

fn draw_list(frame: &mut Frame, app: &mut AppState, area: Rect, hit_regions: &mut Vec<HitRegion>) {
    let block = Block::default().borders(Borders::ALL).title(" Threads ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if app.rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No unresolved feedback threads found.\nPress `a` to include resolved threads or `f` to refresh.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let visible_height = inner.height as usize;
    if let Some(sel) = app.selected_row {
        if sel < app.list_scroll {
            app.list_scroll = sel;
        } else if sel >= app.list_scroll + visible_height {
            app.list_scroll = sel + 1 - visible_height;
        }
    }

    for (offset, row_idx) in (app.list_scroll..app.rows.len())
        .take(visible_height)
        .enumerate()
    {
        let y = inner.y + offset as u16;
        let row_rect = Rect::new(inner.x, y, inner.width, 1);
        let is_selected = app.selected_row == Some(row_idx);

        match &app.rows[row_idx] {
            ListRow::Header(title) => {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        format!("▸ {title}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
                    row_rect,
                );
            }
            ListRow::ReviewComment {
                comment_index,
                comment,
            } => {
                let preview = compact_preview(&comment.body, 36);
                let base_style = if is_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let prefix = if is_selected { "› " } else { "  " };
                let mut spans = vec![
                    Span::styled(prefix, base_style),
                    Span::styled(
                        format!("#{} ", comment_index + 1),
                        base_style.fg(Color::Yellow),
                    ),
                    Span::styled(
                        format!("{}:{} ", comment.path, comment.line),
                        base_style.add_modifier(Modifier::BOLD),
                    ),
                ];
                if comment.agent {
                    spans.push(Span::styled("[agent] ", base_style.fg(Color::Magenta)));
                }
                spans.push(Span::styled(preview, base_style.fg(Color::Gray)));
                let line = Line::from(spans);
                frame.render_widget(Paragraph::new(line), row_rect);
                hit_regions.push(HitRegion {
                    rect: row_rect,
                    action: HitAction::SelectRow(row_idx),
                });
            }
            ListRow::Thread(item) => {
                let draft = app.drafts.get(&(item.pr_number, item.thread.id.clone()));
                let has_reply = draft
                    .and_then(|d| d.reply_body.as_deref())
                    .is_some_and(|b| !b.trim().is_empty());
                let staged_resolve = draft.is_some_and(|d| d.resolve);

                let btn_label = if staged_resolve {
                    "[✓Resolve]"
                } else {
                    "[Resolve]"
                };
                let btn_width = btn_label.chars().count() as u16;
                let base_style = if is_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };

                let loc = match item.thread.effective_line() {
                    Some(l) => format!("{}:{l}", item.thread.path),
                    None => item.thread.path.clone(),
                };
                let author = item
                    .thread
                    .comments
                    .first()
                    .map(|c| c.author.as_str())
                    .unwrap_or("unknown");

                let mut spans = vec![
                    Span::styled(if is_selected { "› " } else { "  " }, base_style),
                    Span::styled(format!("[{}] ", item.index), base_style.fg(Color::Yellow)),
                    Span::styled(format!("{loc} "), base_style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!("@{author} "), base_style.fg(Color::DarkGray)),
                ];
                if item.thread.is_outdated {
                    spans.push(Span::styled("[outdated] ", base_style.fg(Color::Yellow)));
                }
                if item.thread.is_resolved {
                    spans.push(Span::styled("[resolved] ", base_style.fg(Color::Green)));
                }
                if has_reply {
                    if draft.is_some_and(|d| d.reply_agent) {
                        spans.push(Span::styled(
                            "[agent reply] ",
                            base_style.fg(Color::Magenta),
                        ));
                    } else {
                        spans.push(Span::styled("[reply] ", base_style.fg(Color::Cyan)));
                    }
                }

                let text_width = inner.width.saturating_sub(btn_width + 1);
                let text_rect = Rect::new(inner.x, y, text_width, 1);
                frame.render_widget(Paragraph::new(Line::from(spans)), text_rect);
                hit_regions.push(HitRegion {
                    rect: row_rect,
                    action: HitAction::SelectRow(row_idx),
                });

                if inner.width > btn_width + 4 && !item.thread.is_resolved {
                    let btn_rect = Rect::new(inner.x + inner.width - btn_width, y, btn_width, 1);
                    let btn_style = if staged_resolve {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Yellow)
                    };
                    frame.render_widget(
                        Paragraph::new(Span::styled(btn_label, btn_style)),
                        btn_rect,
                    );
                    hit_regions.push(HitRegion {
                        rect: btn_rect,
                        action: HitAction::ToggleRowResolve(row_idx),
                    });
                }
            }
        }
    }
}

fn draw_detail(
    frame: &mut Frame,
    client: &Client,
    app: &AppState,
    area: Rect,
    hit_regions: &mut Vec<HitRegion>,
) {
    let block = Block::default().borders(Borders::ALL).title(" Detail ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 || inner.width < 10 {
        return;
    }

    let Some(selected) = app.selected() else {
        frame.render_widget(
            Paragraph::new("Select a thread to inspect code and comments.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    let (path, start_line, end_line, head_sha) = match selected {
        ListRow::Thread(item) => {
            let line = item.thread.effective_line();
            let start = item.thread.start_line.or(line);
            (
                item.thread.path.as_str(),
                start,
                line,
                Some(item.head_sha.as_str()),
            )
        }
        ListRow::ReviewComment { comment, .. } => (
            comment.path.as_str(),
            Some(comment.start_line),
            Some(comment.line),
            app.review_pr.as_ref().map(|p| p.head_sha.as_str()),
        ),
        ListRow::Header(_) => return,
    };

    let snippet_lines = load_code_snippet(client, path, start_line, end_line, head_sha);
    let snippet_box_height = (snippet_lines.len() as u16 + 2)
        .min(inner.height / 2)
        .max(3);

    let [snippet_area, actions_area, comments_area] = Layout::vertical([
        Constraint::Length(snippet_box_height),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    // 1. Code snippet box (clickable to open in Zed; focusable with Tab/Space).
    let snippet_focused = app.focus == FocusTarget::Snippet;
    let snippet_title = format!(
        " {} {} ",
        path,
        if snippet_focused {
            "[Space/Click: Open in Zed]"
        } else {
            "(click to open)"
        }
    );
    let snippet_border_style = if snippet_focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let snippet_block = Block::default()
        .borders(Borders::ALL)
        .border_style(snippet_border_style)
        .title(snippet_title);
    let snippet_inner = snippet_block.inner(snippet_area);
    frame.render_widget(snippet_block, snippet_area);

    if snippet_lines.is_empty() {
        frame.render_widget(
            Paragraph::new(" (file preview unavailable — press Space or click to open)")
                .style(Style::default().fg(Color::DarkGray)),
            snippet_inner,
        );
        hit_regions.push(HitRegion {
            rect: snippet_area,
            action: HitAction::OpenSnippetLine(end_line.unwrap_or(1)),
        });
    } else {
        for (idx, sline) in snippet_lines
            .iter()
            .take(snippet_inner.height as usize)
            .enumerate()
        {
            let row_rect = Rect::new(
                snippet_inner.x,
                snippet_inner.y + idx as u16,
                snippet_inner.width,
                1,
            );
            let mut spans = Vec::with_capacity(sline.spans.len() + 2);
            let marker_style = if sline.highlighted {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(
                format!(
                    "{} {:>4} │ ",
                    if sline.highlighted { ">" } else { " " },
                    sline.line_no
                ),
                marker_style,
            ));
            spans.extend(sline.spans.iter().cloned());
            frame.render_widget(Paragraph::new(Line::from(spans)), row_rect);
            hit_regions.push(HitRegion {
                rect: row_rect,
                action: HitAction::OpenSnippetLine(sline.line_no),
            });
        }
    }

    // 2. Action bar (keyboard-focusable via Tab/h/l + Space, and clickable).
    draw_action_bar(frame, app, selected, actions_area, hit_regions);

    // 3. Thread comments / staged reply / review comment body.
    let mut body_lines: Vec<Line> = Vec::new();
    match selected {
        ListRow::Thread(item) => {
            for comment in &item.thread.comments {
                body_lines.push(Line::from(vec![
                    Span::styled(
                        format!("@{}", comment.author),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", comment.created_at),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                for text_line in comment.body.lines() {
                    body_lines.push(Line::from(format!("  {}", plain_status(text_line))));
                }
                body_lines.push(Line::from(""));
            }
            if let Some(draft) = app.drafts.get(&(item.pr_number, item.thread.id.clone())) {
                if let Some(reply) = draft.reply_body.as_deref()
                    && !reply.trim().is_empty()
                {
                    let mut header_spans = vec![Span::styled(
                        "Staged Reply (local draft)",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )];
                    if draft.reply_agent {
                        header_spans.push(Span::styled(
                            " [agent]",
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }
                    header_spans.push(Span::styled(
                        ":",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                    body_lines.push(Line::from(header_spans));
                    for text_line in reply.lines() {
                        body_lines.push(Line::styled(
                            format!("  {}", plain_status(text_line)),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    body_lines.push(Line::from(""));
                }
                if draft.resolve {
                    body_lines.push(Line::styled(
                        "✓ Marked to resolve on publish (Ctrl-Shift-X).",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
        }
        ListRow::ReviewComment {
            comment_index,
            comment,
        } => {
            let mut header_spans = vec![Span::styled(
                format!(
                    "Staged Review Comment #{} ({}:{}–{})",
                    comment_index + 1,
                    comment.path,
                    comment.start_line,
                    comment.line
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )];
            if comment.agent {
                header_spans.push(Span::styled(
                    " [agent]",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            header_spans.push(Span::styled(
                ":",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            body_lines.push(Line::from(header_spans));
            for text_line in comment.body.lines() {
                body_lines.push(Line::from(format!("  {}", plain_status(text_line))));
            }
        }
        ListRow::Header(_) => {}
    }

    frame.render_widget(
        Paragraph::new(Text::from(body_lines)).wrap(Wrap { trim: false }),
        comments_area,
    );
}

fn draw_action_bar(
    frame: &mut Frame,
    app: &AppState,
    selected: &ListRow,
    area: Rect,
    hit_regions: &mut Vec<HitRegion>,
) {
    let mut buttons: Vec<(&str, FocusTarget, PendingAction, Color)> = Vec::new();
    match selected {
        ListRow::Thread(item) => {
            let draft = app.drafts.get(&(item.pr_number, item.thread.id.clone()));
            let staged_resolve = draft.is_some_and(|d| d.resolve);
            let has_staged = draft.is_some_and(FeedbackDraft::has_staged_work);
            buttons.push((
                "[Open in Zed]",
                FocusTarget::Snippet,
                PendingAction::OpenCode {
                    checkout: false,
                    line_override: None,
                },
                Color::Cyan,
            ));
            buttons.push((
                "[Reply]",
                FocusTarget::PrimaryEdit,
                PendingAction::EditOrReply,
                Color::Green,
            ));
            buttons.push((
                if staged_resolve {
                    "[Unresolve]"
                } else {
                    "[Resolve]"
                },
                FocusTarget::Resolve,
                PendingAction::ToggleResolve,
                Color::Yellow,
            ));
            if has_staged {
                buttons.push((
                    "[Clear]",
                    FocusTarget::RemoveOrClear,
                    PendingAction::ClearOrRemove,
                    Color::Red,
                ));
            }
        }
        ListRow::ReviewComment { .. } => {
            buttons.push((
                "[Open in Zed]",
                FocusTarget::Snippet,
                PendingAction::OpenCode {
                    checkout: false,
                    line_override: None,
                },
                Color::Cyan,
            ));
            buttons.push((
                "[Edit]",
                FocusTarget::PrimaryEdit,
                PendingAction::EditOrReply,
                Color::Green,
            ));
            buttons.push((
                "[Remove]",
                FocusTarget::RemoveOrClear,
                PendingAction::ClearOrRemove,
                Color::Red,
            ));
        }
        ListRow::Header(_) => return,
    }

    let mut x = area.x;
    for (label, target, action, color) in buttons {
        let width = label.chars().count() as u16;
        if x.saturating_add(width) > area.x.saturating_add(area.width) {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        let style = if app.focus == target {
            Style::default()
                .fg(color)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        };
        frame.render_widget(Paragraph::new(Span::styled(label, style)), rect);
        hit_regions.push(HitRegion {
            rect,
            action: HitAction::Trigger(action),
        });
        x = x.saturating_add(width + 2);
    }
}

fn load_code_snippet(
    client: &Client,
    relative_path: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
    head_sha: Option<&str>,
) -> Vec<SnippetLine> {
    let content = feedback::resolve_open_path(&client.root, relative_path)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .or_else(|| {
            head_sha
                .filter(|s| !s.is_empty())
                .and_then(|sha| client.blob(sha, relative_path).ok())
                .and_then(|bytes| String::from_utf8(bytes).ok())
        });
    let Some(content) = content else {
        return Vec::new();
    };
    let all_lines: Vec<&str> = content.lines().collect();
    if all_lines.is_empty() {
        return Vec::new();
    }
    let target_end = end_line.unwrap_or(1).clamp(1, all_lines.len() as u32) as usize;
    let target_start = start_line
        .unwrap_or(target_end as u32)
        .clamp(1, target_end as u32) as usize;
    let first = target_start.saturating_sub(SNIPPET_CONTEXT).max(1);
    let last = target_end
        .saturating_add(SNIPPET_CONTEXT)
        .min(all_lines.len());
    let ext = Path::new(relative_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    (first..=last)
        .map(|line_no| {
            let highlighted = (target_start..=target_end).contains(&line_no);
            let spans = highlight_code_line(all_lines[line_no - 1], ext);
            SnippetLine {
                line_no: line_no as u32,
                highlighted,
                spans,
            }
        })
        .collect()
}

/// Lightweight, dependency-free syntax highlighting for common languages.
pub(crate) fn highlight_code_line(line: &str, ext: &str) -> Vec<Span<'static>> {
    let clean = plain_status(line);
    let trimmed = clean.trim_start();
    let hash_comments = matches!(ext, "py" | "sh" | "zsh" | "bash" | "toml" | "yaml" | "yml");
    let slash_comments = matches!(
        ext,
        "rs" | "ts" | "tsx" | "js" | "jsx" | "go" | "c" | "h" | "cpp" | "cc" | "java" | "swift"
    );

    if (slash_comments
        && (trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')))
        || (hash_comments && trimmed.starts_with('#'))
        || (ext == "sql" && trimmed.starts_with("--"))
    {
        return vec![Span::styled(clean, Style::default().fg(Color::DarkGray))];
    }

    let keywords: &[&str] = match ext {
        "rs" => &[
            "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "for", "in", "if",
            "else", "match", "return", "use", "mod", "const", "static", "async", "await", "where",
            "type", "Self", "self", "true", "false", "while", "loop", "break", "continue",
        ],
        "ts" | "tsx" | "js" | "jsx" => &[
            "function",
            "const",
            "let",
            "var",
            "return",
            "if",
            "else",
            "for",
            "while",
            "import",
            "export",
            "from",
            "type",
            "interface",
            "class",
            "async",
            "await",
            "new",
            "true",
            "false",
            "null",
            "undefined",
            "switch",
            "case",
            "try",
            "catch",
            "throw",
        ],
        "py" => &[
            "def", "class", "return", "if", "elif", "else", "for", "in", "while", "import", "from",
            "as", "with", "try", "except", "raise", "pass", "None", "True", "False", "async",
            "await", "and", "or", "not",
        ],
        "go" => &[
            "func",
            "var",
            "const",
            "type",
            "struct",
            "interface",
            "if",
            "else",
            "for",
            "range",
            "return",
            "package",
            "import",
            "defer",
            "go",
            "select",
            "case",
            "switch",
            "true",
            "false",
            "nil",
        ],
        "sh" | "zsh" | "bash" => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "case", "esac", "local",
            "export", "return", "function",
        ],
        _ => &[],
    };

    let mut spans = Vec::new();
    let chars: Vec<char> = clean.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Inline comment
        if slash_comments && i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(rest, Style::default().fg(Color::DarkGray)));
            break;
        }
        if hash_comments && chars[i] == '#' {
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(rest, Style::default().fg(Color::DarkGray)));
            break;
        }
        // String literal
        if chars[i] == '"' || chars[i] == '\'' || chars[i] == '`' {
            let quote = chars[i];
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            spans.push(Span::styled(text, Style::default().fg(Color::Green)));
            continue;
        }
        // Number literal
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            spans.push(Span::styled(text, Style::default().fg(Color::Yellow)));
            continue;
        }
        // Identifier or keyword
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let style = if keywords.contains(&word.as_str()) {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD)
            } else if word.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            spans.push(Span::styled(word, style));
            continue;
        }
        // Other punctuation/whitespace
        let start = i;
        i += 1;
        while i < chars.len()
            && !chars[i].is_ascii_alphanumeric()
            && chars[i] != '_'
            && chars[i] != '"'
            && chars[i] != '\''
            && chars[i] != '`'
            && !(slash_comments && chars[i] == '/')
            && !(hash_comments && chars[i] == '#')
        {
            i += 1;
        }
        let text: String = chars[start..i].iter().collect();
        spans.push(Span::raw(text));
    }

    spans
}

fn compact_preview(body: &str, max_chars: usize) -> String {
    let first_line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let clean = plain_status(first_line);
    let mut out: String = clean.chars().take(max_chars).collect();
    if clean.chars().count() > max_chars {
        out.push('…');
    }
    out
}

pub fn print_plain_summary(index: &FeedbackIndex, feedback_dir: &Path, include_all: bool) {
    let drafts = feedback::load_draft_map(feedback_dir);
    let visible = feedback::visible_threads(index, include_all);
    if visible.is_empty() {
        println!("No unresolved review threads found.");
        return;
    }
    let mut current_group: Option<usize> = None;
    for item in visible {
        if current_group != Some(item.group_index) {
            current_group = Some(item.group_index);
            if current_group != Some(0) {
                println!();
            }
            println!(
                "{} — PR #{} {} ({})",
                item.bookmark,
                item.pr_number,
                plain_status(&item.pr_title),
                item.pr_url
            );
        }
        let loc = match item.thread.effective_line() {
            Some(l) => format!("{}:{l}", item.thread.path),
            None => item.thread.path.clone(),
        };
        let author = item
            .thread
            .comments
            .first()
            .map(|c| c.author.as_str())
            .unwrap_or("unknown");
        let preview = item
            .thread
            .comments
            .last()
            .map(|c| compact_preview(&c.body, 72))
            .unwrap_or_default();
        let mut tags = Vec::new();
        if item.thread.is_outdated {
            tags.push("[outdated]");
        }
        if item.thread.is_resolved {
            tags.push("[resolved]");
        }
        if let Some(draft) = drafts.get(&(item.pr_number, item.thread.id.clone())) {
            if draft
                .reply_body
                .as_deref()
                .is_some_and(|b| !b.trim().is_empty())
            {
                if draft.reply_agent {
                    tags.push("[agent reply staged]");
                } else {
                    tags.push("[reply staged]");
                }
            }
            if draft.resolve {
                tags.push("[resolve staged]");
            }
            if draft.attempt.is_some() {
                tags.push("[recover needed]");
            }
        }
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!(" {}", tags.join(" "))
        };
        println!("  [{}] {loc} (@{author}){tag_str} — {preview}", item.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::IndexedGroup;
    use crate::model::{ReviewThread, ThreadComment};

    fn sample_index() -> FeedbackIndex {
        FeedbackIndex {
            version: 1,
            host: "github.com".into(),
            repo: "org/repo".into(),
            groups: vec![IndexedGroup {
                bookmark: "feat-a".into(),
                change_id: "chg1".into(),
                commit_id: "com1".into(),
                pr_number: 10,
                pr_title: "Add feature".into(),
                pr_url: "https://github.com/org/repo/pull/10".into(),
                head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                threads: vec![
                    ReviewThread {
                        id: "PRRT_1".into(),
                        is_resolved: false,
                        is_outdated: false,
                        is_collapsed: false,
                        path: "src/lib.rs".into(),
                        line: Some(10),
                        original_line: Some(10),
                        start_line: None,
                        diff_side: "RIGHT".into(),
                        comments: vec![ThreadComment {
                            id: "C1".into(),
                            database_id: Some(1),
                            author: "alice".into(),
                            body: "Rename variable".into(),
                            created_at: "2026-01-01T00:00:00Z".into(),
                            url: String::new(),
                        }],
                        pr_number: 10,
                        bookmark: "feat-a".into(),
                        head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    },
                    ReviewThread {
                        id: "PRRT_2".into(),
                        is_resolved: false,
                        is_outdated: true,
                        is_collapsed: false,
                        path: "src/main.rs".into(),
                        line: None,
                        original_line: Some(25),
                        start_line: None,
                        diff_side: "RIGHT".into(),
                        comments: vec![ThreadComment {
                            id: "C2".into(),
                            database_id: Some(2),
                            author: "bob".into(),
                            body: "Check error case".into(),
                            created_at: "2026-01-01T00:00:00Z".into(),
                            url: String::new(),
                        }],
                        pr_number: 10,
                        bookmark: "feat-a".into(),
                        head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn vim_navigation_and_focus_cycling_trigger_active_item_with_space() {
        let mut app = AppState::new(sample_index(), HashMap::new(), None, Vec::new(), false);
        assert_eq!(app.selected_row, Some(1));
        assert_eq!(app.focus, FocusTarget::Snippet);

        let mut pending = None;
        assert!(!handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut pending
        ));
        assert_eq!(
            pending,
            Some(PendingAction::OpenCode {
                checkout: false,
                line_override: None
            })
        );

        // Move focus to Reply and press Space.
        app.cycle_focus(false);
        assert_eq!(app.focus, FocusTarget::PrimaryEdit);
        pending = None;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut pending,
        );
        assert_eq!(pending, Some(PendingAction::EditOrReply));

        // Move focus to Resolve and press Space.
        app.cycle_focus(false);
        assert_eq!(app.focus, FocusTarget::Resolve);
        pending = None;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut pending,
        );
        assert_eq!(pending, Some(PendingAction::ToggleResolve));

        // j/k moves between threads skipping headers.
        app.move_selection(1);
        assert_eq!(app.selected_row, Some(2));
        app.move_selection(-1);
        assert_eq!(app.selected_row, Some(1));
    }

    #[test]
    fn syntax_highlighter_styles_keywords_strings_and_comments() {
        let spans = highlight_code_line("pub fn hello() -> String { \"hi\" } // note", "rs");
        assert!(
            spans
                .iter()
                .any(|s| s.content == "pub" && s.style.fg == Some(Color::Magenta))
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content == "String" && s.style.fg == Some(Color::Cyan))
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content == "\"hi\"" && s.style.fg == Some(Color::Green))
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content == "// note" && s.style.fg == Some(Color::DarkGray))
        );
    }
}
