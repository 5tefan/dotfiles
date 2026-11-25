//! Read-only PR log with bounded refresh frequency. Network reads run off the UI
//! thread, so a slow GitHub response never prevents scrolling or closing the view.

use std::io;
use std::sync::{atomic::Ordering, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::log_view::{Screen, plain_status};
use crate::stack::{RenderedStack, Stack};

const REFRESH: Duration = Duration::from_secs(30);
const HELP: &str = "j/k/wheel: scroll · h/l: pan · @: current · r: refresh · q: quit";
const STATUS: &str =
    "Ctrl-Shift-J/K: parent/child PR · GitHub refresh: 30s · Shift-drag: select text";

#[derive(Default)]
struct View {
    log: Option<RenderedStack>,
    identity: String,
    top: usize,
    left: usize,
    width: usize,
    height: usize,
    reveal_when_sized: bool,
}

impl View {
    fn apply(&mut self, identity: String, log: RenderedStack) {
        let changed = self.identity != identity || self.log.is_none();
        // Keep the same position relative to @ when new PRs appear above it.
        if !changed && let Some(previous) = &self.log {
            self.top = self
                .top
                .saturating_add(log.current_row)
                .saturating_sub(previous.current_row);
        }
        self.identity = identity;
        self.log = Some(log);
        if changed {
            self.show_current();
        }
        self.clamp();
    }

    fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        if self.reveal_when_sized && width > 0 && height > 0 {
            self.show_current();
        }
        self.clamp();
    }

    fn clamp(&mut self) {
        let Some(log) = &self.log else { return };
        self.top = self
            .top
            .min(log.lines.len().saturating_sub(self.height.max(1)));
        let width = log.lines.iter().map(|line| line.width()).max().unwrap_or(0);
        self.left = self.left.min(width.saturating_sub(self.width.max(1)));
    }

    fn scroll(&mut self, rows: isize, columns: isize) {
        self.top = self.top.saturating_add_signed(rows);
        self.left = self.left.saturating_add_signed(columns);
        self.clamp();
    }

    fn show_current(&mut self) {
        self.reveal_when_sized = self.height == 0 || self.width == 0;
        if let Some(log) = &self.log {
            self.top = log.current_row.saturating_sub(self.height / 2);
            self.left = log.lines[log.current_row]
                .spans
                .iter()
                .take_while(|span| span.content != "@")
                .map(|span| span.width())
                .sum::<usize>();
        }
        self.clamp();
    }
}

/// The caller must route non-terminal input/output to the one-shot renderer.
/// `load` must be read-only and must not hold a review workspace lock.
pub fn run(load: impl FnMut() -> Result<Stack> + Send + 'static) -> Result<()> {
    run_with_status(load, || None)
}

pub fn run_with_status(
    mut load: impl FnMut() -> Result<Stack> + Send + 'static,
    mut preparation_status: impl FnMut() -> Option<String>,
) -> Result<()> {
    let screen = Screen::enter().context("Cannot enter the interactive PR stack")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let (requests, jobs) = mpsc::channel();
    let (results, replies) = mpsc::channel();
    std::thread::Builder::new()
        .name("pr-stack".into())
        .spawn(move || {
            while jobs.recv().is_ok() {
                if results.send(load()).is_err() {
                    break;
                }
            }
        })?;

    let mut view = View::default();
    let mut title = String::from("PR stack");
    let mut status = String::from("Loading PR stack…");
    let mut busy = false;
    let mut refresh_requested = false;
    let mut next_refresh = Instant::now();
    let mut dirty = true;
    let mut body = Rect::default();
    let mut preparation = None;
    let mut next_status = Instant::now();
    loop {
        if Instant::now() >= next_status {
            let current = preparation_status();
            if current != preparation {
                preparation = current;
                dirty = true;
            }
            next_status = Instant::now() + Duration::from_millis(250);
        }
        match replies.try_recv() {
            Ok(result) => {
                busy = false;
                match result {
                    Ok(stack) => {
                        let pr = &stack.current().pr;
                        title = format!("PR stack — {}/{}", pr.host, pr.repo);
                        view.apply(pr.url.clone(), stack.render_log());
                        status = STATUS.into();
                    }
                    Err(error) => status = format!("Refresh failed: {error:#}"),
                }
                next_refresh = Instant::now() + REFRESH;
                dirty = true;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("The PR stack worker stopped unexpectedly")
            }
        }
        if screen.stopped.load(Ordering::Relaxed) {
            break;
        }
        if !busy && (refresh_requested || Instant::now() >= next_refresh) {
            requests.send(())?;
            refresh_requested = false;
            busy = true;
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| {
                let area = frame.area();
                body = Rect::new(
                    area.x,
                    area.y.saturating_add(1),
                    area.width,
                    area.height.saturating_sub(3),
                );
                view.resize(body.width as usize, body.height as usize);
                let count = view.log.as_ref().map_or(0, |log| log.lines.len());
                let heading = format!(
                    "{} · {} · lines {}–{}/{}",
                    plain_status(&title),
                    if busy { "refreshing…" } else { "live" },
                    if count == 0 { 0 } else { view.top + 1 },
                    (view.top + view.height).min(count),
                    count
                );
                frame.render_widget(
                    Paragraph::new(heading).style(Style::default().add_modifier(Modifier::BOLD)),
                    Rect::new(area.x, area.y, area.width, area.height.min(1)),
                );
                if let Some(log) = &view.log {
                    let lines: Vec<_> = log
                        .lines
                        .iter()
                        .skip(view.top)
                        .take(view.height)
                        .cloned()
                        .collect();
                    frame.render_widget(
                        Paragraph::new(Text::from(lines))
                            .scroll((0, view.left.min(u16::MAX as usize) as u16)),
                        body,
                    );
                }
                if area.height >= 2 {
                    frame.render_widget(
                        Paragraph::new(plain_status(&match &preparation {
                            Some(preparation) => format!("{status} · {preparation}"),
                            None => status.clone(),
                        })),
                        Rect::new(area.x, area.y + area.height - 2, area.width, 1),
                    );
                }
                if area.height >= 3 {
                    frame.render_widget(
                        Paragraph::new(HELP).style(Style::default().fg(Color::DarkGray)),
                        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
                    );
                }
            })?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => dirty = true,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                dirty = true;
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
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
                    KeyCode::Char('@') => view.show_current(),
                    KeyCode::Char('r') => refresh_requested = true,
                    _ => {}
                }
            }
            Event::Mouse(mouse) if body.contains((mouse.column, mouse.row).into()) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => view.scroll(-3, 0),
                    MouseEventKind::ScrollDown => view.scroll(3, 0),
                    MouseEventKind::ScrollLeft => view.scroll(0, -4),
                    MouseEventKind::ScrollRight => view.scroll(0, 4),
                    _ => continue,
                }
                dirty = true;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;

    fn log(current_row: usize) -> RenderedStack {
        RenderedStack {
            lines: (0..60)
                .map(|row| {
                    Line::from(if row == current_row {
                        "@ #17"
                    } else {
                        "│ text"
                    })
                })
                .collect(),
            current_row,
        }
    }

    #[test]
    fn reveals_current_on_first_draw_but_not_on_refresh_or_resize() {
        let mut view = View::default();
        view.apply("pr17".into(), log(30));
        view.resize(80, 10);
        assert_eq!(view.top, 25);
        view.scroll(-10, 0);
        view.apply("pr17".into(), log(30));
        view.resize(100, 12);
        assert_eq!(view.top, 15);
        view.apply("pr17".into(), log(33));
        assert_eq!(view.top, 18);
        view.apply("pr18".into(), log(40));
        assert_eq!(view.top, 34);
    }

    #[test]
    fn collapsed_and_tiny_viewports_are_safe() {
        let mut view = View::default();
        view.show_current();
        view.scroll(isize::MAX, isize::MAX);
        view.apply("pr17".into(), log(59));
        view.resize(0, 0);
        view.resize(1, 1);
        assert_eq!(view.top, 59);
        view.show_current();
        view.scroll(isize::MIN, isize::MIN);
        assert_eq!((view.top, view.left), (0, 0));
    }
}
