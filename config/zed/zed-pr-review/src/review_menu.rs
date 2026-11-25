//! Choose a review action without changing the draft, GitHub, or the checkout.

use std::io::{self, BufRead, IsTerminal, Write};
use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::log_view::{Screen, plain_status};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewAction {
    /// Archive and clear the local draft for this PR, staying on the review stack.
    Cancel,
    Comment,
    Approve,
    RequestChanges,
    /// Abandon all local review/pr-* revisions and return to previous @.
    CloseStack,
}

const CHOICES: [(ReviewAction, &str, &str); 5] = [
    (
        ReviewAction::Cancel,
        "1  Abandon draft",
        "Archive and clear local draft for this PR; stay on stack.",
    ),
    (
        ReviewAction::Comment,
        "2  Comment",
        "Submit a COMMENT-only review; stay on stack.",
    ),
    (
        ReviewAction::Approve,
        "3  Approve",
        "Submit an APPROVE review; stay on stack.",
    ),
    (
        ReviewAction::RequestChanges,
        "4  Request changes",
        "Submit a REQUEST_CHANGES review; stay on stack.",
    ),
    (
        ReviewAction::CloseStack,
        "5  Close review stack",
        "Abandon all review/pr-* commits and return to previous @.",
    ),
];

/// `None` dismisses the menu without abandoning anything. Only an explicit choice
/// returns an action. The terminal is restored before the caller receives it.
pub fn choose(pr_label: &str, pending_comments: usize) -> Result<Option<ReviewAction>> {
    let label = plain_status(pr_label);
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        choose_terminal(&label, pending_comments)
    } else {
        // Use stdin's shared buffer, not a separate BufReader that could discard
        // input needed by the action after this one-line choice.
        choose_plain(
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
            &label,
            pending_comments,
        )
    }
}

fn choose_plain(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    pending_comments: usize,
) -> Result<Option<ReviewAction>> {
    writeln!(output, "Finish PR review — {label}")?;
    writeln!(output, "{}\n", pending_label(pending_comments))?;
    for (_, title, description) in CHOICES {
        writeln!(output, "{title}\n   {description}")?;
    }
    write!(output, "Choice [1–5] (blank/q cancels and keeps draft): ")?;
    output.flush()?;
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("Cannot read the review choice; no action selected")?;
    match line.trim() {
        "" | "q" | "\u{1b}" | "\u{3}" => Ok(None),
        "1" => Ok(Some(ReviewAction::Cancel)),
        "2" => Ok(Some(ReviewAction::Comment)),
        "3" => Ok(Some(ReviewAction::Approve)),
        "4" => Ok(Some(ReviewAction::RequestChanges)),
        "5" => Ok(Some(ReviewAction::CloseStack)),
        _ => bail!("Invalid review choice: enter 1–5 or leave blank; no action selected"),
    }
}

fn choose_terminal(label: &str, pending_comments: usize) -> Result<Option<ReviewAction>> {
    let screen = Screen::enter().context("Cannot enter the review menu")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run(&mut terminal, &screen, label, pending_comments);
    let result = if screen.stopped.load(Ordering::Relaxed) {
        Ok(None)
    } else {
        result
    };
    drop(terminal);
    drop(screen);
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    screen: &Screen,
    label: &str,
    pending_comments: usize,
) -> Result<Option<ReviewAction>> {
    terminal.clear()?;
    let mut menu = Menu::default();
    let mut dirty = true;
    loop {
        if screen.stopped.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if dirty {
            terminal.draw(|frame| draw(frame, label, pending_comments, menu.selected))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => dirty = true,
            Event::Key(key) => {
                if let ControlFlow::Break(action) = menu.key(key) {
                    return Ok(action);
                }
                dirty = true;
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct Menu {
    selected: Option<usize>,
}

impl Menu {
    fn key(&mut self, key: KeyEvent) -> ControlFlow<Option<ReviewAction>> {
        if key.kind == KeyEventKind::Release {
            return ControlFlow::Continue(());
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ControlFlow::Break(None),
            KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return ControlFlow::Break(None);
            }
            _ => {}
        }
        if key.modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER,
        ) {
            return ControlFlow::Continue(());
        }
        match key.code {
            KeyCode::Up | KeyCode::Left => {
                self.selected = Some(self.selected.map_or(CHOICES.len() - 1, |index| {
                    (index + CHOICES.len() - 1) % CHOICES.len()
                }));
            }
            KeyCode::Down | KeyCode::Right => {
                self.selected = Some(self.selected.map_or(0, |index| (index + 1) % CHOICES.len()));
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                return ControlFlow::Break(self.selected.map(|index| CHOICES[index].0));
            }
            KeyCode::Char(number @ '1'..='5') if key.kind == KeyEventKind::Press => {
                return ControlFlow::Break(Some(CHOICES[number as usize - '1' as usize].0));
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn pending_label(count: usize) -> String {
    format!(
        "{count} pending comment{}",
        if count == 1 { "" } else { "s" }
    )
}

fn centered(area: Rect) -> Rect {
    let width = area.width.min(80);
    let height = area.height.min(20);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw(frame: &mut Frame, label: &str, pending_comments: usize, selected: Option<usize>) {
    let dialog = centered(frame.area());
    frame.render_widget(Clear, dialog);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Finish PR review ")
        .title_alignment(Alignment::Center);
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    let [heading, pending, _, choices, help, dismissal] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(Style::default().add_modifier(Modifier::BOLD)),
        heading,
    );
    frame.render_widget(
        Paragraph::new(pending_label(pending_comments)).alignment(Alignment::Center),
        pending,
    );
    let detailed = choices.height >= 10 && choices.width >= 60;
    let items = CHOICES.iter().map(|(_, title, description)| {
        let mut lines = vec![Line::styled(
            *title,
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if detailed {
            lines.push(Line::from(format!("   {description}")));
        }
        ListItem::new(lines)
    });
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        choices,
        &mut ListState::default().with_selected(selected),
    );
    frame.render_widget(
        Paragraph::new("1–5: choose · ↑/↓: select · Enter: confirm").alignment(Alignment::Center),
        help,
    );
    frame.render_widget(
        Paragraph::new(if selected.is_none() {
            "Enter / Esc / q / Ctrl-C: dismiss; keep draft"
        } else {
            "Esc / q / Ctrl-C: dismiss; keep draft"
        })
        .alignment(Alignment::Center),
        dismissal,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackFinishAction {
    Publish,
    Discard,
    Recover,
}

const FEEDBACK_CHOICES_NORMAL: [(FeedbackFinishAction, &str, &str); 2] = [
    (
        FeedbackFinishAction::Publish,
        "1  Reply & publish feedback",
        "Post staged replies and resolve marked threads on GitHub.",
    ),
    (
        FeedbackFinishAction::Discard,
        "2  Discard staged feedback",
        "Archive and clear local feedback drafts; GitHub stays unchanged.",
    ),
];

const FEEDBACK_CHOICES_WITH_RECOVER: [(FeedbackFinishAction, &str, &str); 3] = [
    (
        FeedbackFinishAction::Publish,
        "1  Reply & publish feedback",
        "Post staged replies and resolve marked threads on GitHub.",
    ),
    (
        FeedbackFinishAction::Discard,
        "2  Discard staged feedback",
        "Archive and clear local feedback drafts; GitHub stays unchanged.",
    ),
    (
        FeedbackFinishAction::Recover,
        "3  Recover uncertain publish",
        "Reconcile previous uncertain feedback publish attempts against GitHub.",
    ),
];

/// Feedback-only finish menu for your own stack (`ctrl-shift-x`).
/// Never shows Approve or Request changes.
pub fn choose_feedback(
    stack_label: &str,
    staged_replies: usize,
    staged_resolves: usize,
    uncertain_attempts: usize,
) -> Result<Option<FeedbackFinishAction>> {
    let label = plain_status(stack_label);
    let summary = feedback_summary_label(staged_replies, staged_resolves, uncertain_attempts);
    let choices: &[(FeedbackFinishAction, &str, &str)] = if uncertain_attempts > 0 {
        &FEEDBACK_CHOICES_WITH_RECOVER
    } else {
        &FEEDBACK_CHOICES_NORMAL
    };
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        choose_feedback_terminal(&label, &summary, choices)
    } else {
        choose_feedback_plain(
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
            &label,
            &summary,
            choices,
        )
    }
}

fn feedback_summary_label(
    staged_replies: usize,
    staged_resolves: usize,
    uncertain_attempts: usize,
) -> String {
    let mut parts = vec![
        format!(
            "{staged_replies} staged repl{}",
            if staged_replies == 1 { "y" } else { "ies" }
        ),
        format!(
            "{staged_resolves} resolve{}",
            if staged_resolves == 1 { "" } else { "s" }
        ),
    ];
    if uncertain_attempts > 0 {
        parts.push(format!("{uncertain_attempts} uncertain"));
    }
    parts.join(" · ")
}

fn choose_feedback_plain(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    summary: &str,
    choices: &[(FeedbackFinishAction, &str, &str)],
) -> Result<Option<FeedbackFinishAction>> {
    writeln!(output, "Finish stack feedback — {label}")?;
    writeln!(output, "{summary}\n")?;
    for (_, title, description) in choices {
        writeln!(output, "{title}\n   {description}")?;
    }
    let max = choices.len();
    write!(
        output,
        "Choice [1–{max}] (blank/q cancels and keeps drafts): "
    )?;
    output.flush()?;
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("Cannot read the feedback choice; no action selected")?;
    match line.trim() {
        "" | "q" | "\u{1b}" | "\u{3}" => Ok(None),
        "1" => Ok(Some(choices[0].0)),
        "2" => Ok(Some(choices[1].0)),
        "3" if choices.len() >= 3 => Ok(Some(choices[2].0)),
        _ => bail!("Invalid choice: enter 1–{max} or leave blank; no action selected"),
    }
}

fn choose_feedback_terminal(
    label: &str,
    summary: &str,
    choices: &[(FeedbackFinishAction, &str, &str)],
) -> Result<Option<FeedbackFinishAction>> {
    let screen = Screen::enter().context("Cannot enter the feedback finish menu")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run_feedback_menu(&mut terminal, &screen, label, summary, choices);
    let result = if screen.stopped.load(Ordering::Relaxed) {
        Ok(None)
    } else {
        result
    };
    drop(terminal);
    drop(screen);
    result
}

fn run_feedback_menu(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    screen: &Screen,
    label: &str,
    summary: &str,
    choices: &[(FeedbackFinishAction, &str, &str)],
) -> Result<Option<FeedbackFinishAction>> {
    terminal.clear()?;
    let mut selected: Option<usize> = None;
    let mut dirty = true;
    loop {
        if screen.stopped.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if dirty {
            terminal.draw(|frame| draw_feedback(frame, label, summary, choices, selected))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => dirty = true,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = Some(selected.map_or(choices.len() - 1, |i| {
                            (i + choices.len() - 1) % choices.len()
                        }));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = Some(selected.map_or(0, |i| (i + 1) % choices.len()));
                    }
                    KeyCode::Enter | KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                        return Ok(selected.map(|i| choices[i].0));
                    }
                    KeyCode::Char(ch)
                        if key.kind == KeyEventKind::Press
                            && !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) =>
                    {
                        if let Some(digit) = ch.to_digit(10) {
                            let idx = digit as usize;
                            if idx >= 1 && idx <= choices.len() {
                                return Ok(Some(choices[idx - 1].0));
                            }
                        }
                    }
                    _ => {}
                }
                dirty = true;
            }
            _ => {}
        }
    }
}

fn draw_feedback(
    frame: &mut Frame,
    label: &str,
    summary: &str,
    choices: &[(FeedbackFinishAction, &str, &str)],
    selected: Option<usize>,
) {
    let dialog = centered(frame.area());
    frame.render_widget(Clear, dialog);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Finish stack feedback ")
        .title_alignment(Alignment::Center);
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    let [heading, pending, _, list_area, help, dismissal] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(Style::default().add_modifier(Modifier::BOLD)),
        heading,
    );
    frame.render_widget(
        Paragraph::new(summary).alignment(Alignment::Center),
        pending,
    );
    let detailed = list_area.height >= 6 && list_area.width >= 50;
    let items = choices.iter().map(|(_, title, description)| {
        let mut lines = vec![Line::styled(
            *title,
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if detailed {
            lines.push(Line::from(format!("   {description}")));
        }
        ListItem::new(lines)
    });
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list_area,
        &mut ListState::default().with_selected(selected),
    );
    frame.render_widget(
        Paragraph::new(format!(
            "1–{}: choose · j/k/↑/↓: select · Space/Enter: confirm",
            choices.len()
        ))
        .alignment(Alignment::Center),
        help,
    );
    frame.render_widget(
        Paragraph::new("Esc / q / Ctrl-C: dismiss; keep drafts").alignment(Alignment::Center),
        dismissal,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::io::{Cursor, Read};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn no_default_action_and_dismissal_preserves_the_draft() {
        let mut menu = Menu::default();
        assert_eq!(menu.selected, None);
        assert_eq!(menu.key(key(KeyCode::Enter)), ControlFlow::Break(None));
        for event in [
            key(KeyCode::Esc),
            key(KeyCode::Char('q')),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            menu.selected = Some(0);
            assert_eq!(menu.key(event), ControlFlow::Break(None));
        }
    }

    #[test]
    fn numbers_return_all_five_explicit_actions() {
        for (number, (action, _, _)) in ('1'..='5').zip(CHOICES) {
            assert_eq!(
                Menu::default().key(key(KeyCode::Char(number))),
                ControlFlow::Break(Some(action))
            );
        }
    }

    #[test]
    fn arrows_select_and_wrap_but_do_not_act_until_enter() {
        let mut menu = Menu::default();
        for index in [0, 1, 2, 3, 4, 0] {
            assert_eq!(menu.key(key(KeyCode::Down)), ControlFlow::Continue(()));
            assert_eq!(menu.selected, Some(index));
            assert_eq!(
                menu.key(key(KeyCode::Enter)),
                ControlFlow::Break(Some(CHOICES[index].0))
            );
        }
        for index in [4, 3, 2, 1, 0, 4] {
            assert_eq!(menu.key(key(KeyCode::Up)), ControlFlow::Continue(()));
            assert_eq!(menu.selected, Some(index));
        }
        let mut menu = Menu::default();
        assert_eq!(menu.key(key(KeyCode::Up)), ControlFlow::Continue(()));
        assert_eq!(menu.selected, Some(4));
    }

    #[test]
    fn releases_repeats_and_modified_numbers_do_not_choose() {
        for event in [
            KeyEvent::new_with_kind(
                KeyCode::Char('1'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            KeyEvent::new_with_kind(KeyCode::Char('3'), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
            key(KeyCode::Char('x')),
        ] {
            let mut menu = Menu { selected: Some(0) };
            assert_eq!(menu.key(event), ControlFlow::Continue(()));
            assert_eq!(menu.selected, Some(0));
        }
    }

    #[test]
    fn plain_menu_reads_only_one_line_for_every_choice() {
        for (number, (action, _, _)) in ('1'..='5').zip(CHOICES) {
            let mut input = Cursor::new(format!(" {number} \r\nReview body\nAnother line\n"));
            let mut output = Vec::new();
            assert_eq!(
                choose_plain(&mut input, &mut output, "PR #42", 2).unwrap(),
                Some(action)
            );
            let mut remaining = String::new();
            input.read_to_string(&mut remaining).unwrap();
            assert_eq!(remaining, "Review body\nAnother line\n");
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("2 pending comments"));
            for (_, title, description) in CHOICES {
                assert!(output.contains(title));
                assert!(output.contains(description));
            }
        }
    }

    #[test]
    fn plain_blank_eof_and_escape_cancel_without_reading_further() {
        for first in ["", "\n", " \t\r\n", "q\n", "\u{1b}\n", "\u{3}\n"] {
            let following = if first.is_empty() {
                ""
            } else {
                "Do not consume\n"
            };
            let mut input = Cursor::new(format!("{first}{following}"));
            assert_eq!(
                choose_plain(&mut input, &mut Vec::new(), "PR", 0).unwrap(),
                None
            );
            let mut remaining = String::new();
            input.read_to_string(&mut remaining).unwrap();
            assert_eq!(remaining, following);
        }
    }

    #[test]
    fn plain_invalid_choice_errors_without_retrying_or_echoing_input() {
        for first in ["0", "6", "approve", "1 2", "\u{1b}[31m"] {
            let mut input = Cursor::new(format!("{first}\n3\n"));
            let error = choose_plain(&mut input, &mut Vec::new(), "PR", 1).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Invalid review choice: enter 1–5 or leave blank; no action selected"
            );
            let mut remaining = String::new();
            input.read_to_string(&mut remaining).unwrap();
            assert_eq!(remaining, "3\n");
        }
    }

    #[test]
    fn rendering_is_centered_sanitized_and_safe_in_small_terminals() {
        let label = plain_status("PR #42: untrusted\n\r\t\u{1b}[31m title");
        assert!(!label.chars().any(char::is_control));
        assert_eq!(pending_label(1), "1 pending comment");
        assert_eq!(
            centered(Rect::new(2, 4, 120, 40)),
            Rect::new(22, 14, 80, 20)
        );
        for (width, height) in [(120, 40), (80, 20), (40, 12), (10, 4), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &label, 2, None)).unwrap();
            terminal
                .draw(|frame| draw(frame, &label, 2, Some(3)))
                .unwrap();
            if width >= 80 && height >= 20 {
                let buffer = terminal.backend().buffer();
                let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                assert!(text.contains(&label));
                assert!(text.contains("2 pending comments"));
                for (_, title, description) in CHOICES {
                    assert!(text.contains(title));
                    assert!(text.contains(description));
                }
            }
        }
    }

    #[test]
    fn feedback_menu_excludes_approve_and_request_changes() {
        let mut input = Cursor::new("1\n");
        let mut output = Vec::new();
        let choice = choose_feedback_plain(
            &mut input,
            &mut output,
            "org/repo",
            "2 staged replies · 1 resolve",
            &FEEDBACK_CHOICES_NORMAL,
        )
        .unwrap();
        assert_eq!(choice, Some(FeedbackFinishAction::Publish));
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Reply & publish feedback"));
        assert!(!rendered.contains("Approve"));
        assert!(!rendered.contains("Request changes"));
    }
}
