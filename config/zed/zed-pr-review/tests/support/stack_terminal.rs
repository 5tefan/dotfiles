//! Offline PR-stack terminal regressions, with the CLI fixture environment intact.
#![cfg(unix)]

use super::pty::{self, MOUSE_MODES, Screen, Terminal};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const PULLS: &str = "repos/acme/widgets/pulls?state=open&per_page=100";

fn env_path(name: &str) -> Result<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("Missing {name}"))
}

fn calls() -> Result<Vec<Value>> {
    fs::read_to_string(env_path("ZPR_TEST_GH_LOG")?)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn listing_count() -> Result<usize> {
    Ok(calls()?
        .iter()
        .filter(|call| call["endpoint"] == PULLS)
        .count())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = path.with_extension("pty-next");
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn configure(changes: Value) -> Result<()> {
    let path = env_path("ZPR_TEST_CONTROL")?;
    let mut control: Value = serde_json::from_slice(&fs::read(&path)?)?;
    control
        .as_object_mut()
        .context("Control must be an object")?
        .extend(
            changes
                .as_object()
                .context("Changes must be an object")?
                .clone(),
        );
    // The transport also updates this file. Call only after a completed read,
    // or while listing is paused before the transport loads the control file.
    atomic_write(&path, &serde_json::to_vec(&control)?)
}

struct ListingGate {
    pause: PathBuf,
    started: PathBuf,
}

impl ListingGate {
    fn install() -> Result<Self> {
        let control = env_path("ZPR_TEST_CONTROL")?;
        let root = control.parent().context("Control has no parent")?;
        let offline = root.join("bin/offline-gh");
        // Install a fresh native gh shim so its adjacent configuration remains
        // valid; renaming just the executable would lose that configuration.
        crate::install(&offline, &json!({"tool": "gh"}))?;
        let result = Self {
            pause: root.join("stack-read-pause"),
            started: root.join("stack-read-started"),
        };
        crate::install(
            &root.join("bin/gh"),
            &json!({
                "tool": "gate", "target": offline, "endpoint": PULLS,
                "pause": result.pause, "started": result.started, "timeout_ms": 15000,
            }),
        )?;
        Ok(result)
    }

    fn pause(&self) -> Result<()> {
        remove_if_present(&self.started)?;
        fs::write(&self.pause, "")?;
        Ok(())
    }

    fn release(&self) -> Result<()> {
        remove_if_present(&self.pause)
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl Drop for ListingGate {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn current_visible(screen: &Screen) -> bool {
    screen.body().iter().any(|line| line.starts_with("@ #17 "))
}

fn current_updated(screen: &Screen, title: &str, draft: bool) -> bool {
    current_visible(screen)
        && screen.body().contains(&format!("│ {title}"))
        && screen
            .body()
            .iter()
            .any(|line| line.starts_with("@ #17 topic ") && line.contains("[draft]") == draft)
}

fn other_review_command(binary: &OsString, terminal: &mut Terminal) -> Result<()> {
    // `list` takes the real workspace and draft locks without changing a draft.
    let result = pty::output(Command::new(binary).arg("list"), Duration::from_secs(8))?;
    ensure!(
        result.status.success(),
        "The viewer blocked another review command:\n{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&result.stdout).contains("Keep this private navigation draft"),
        "The second review command did not read the draft"
    );
    ensure!(
        terminal.child.0.try_wait()?.is_none(),
        "The viewer exited during another review task"
    );
    Ok(())
}

fn navigation(terminal: &mut Terminal) -> Result<()> {
    terminal.send(b"\x1b[H")?;
    terminal.wait(|t| Ok(t.screen.view()?.0 == 1), "Home")?;
    ensure!(
        !current_visible(&terminal.screen),
        "Fixture did not put @ below the first page"
    );
    for (key, delta, name) in [
        (&b"j"[..], 1isize, "j"),
        (&b"\x1b[B"[..], 1, "Down"),
        (&b"k"[..], -1, "k"),
        (&b"\x1b[A"[..], -1, "Up"),
    ] {
        let top = terminal.screen.view()?.0;
        terminal.send(key)?;
        terminal.wait(
            |t| {
                Ok(t.screen.view()?.0
                    == top.checked_add_signed(delta).context("Negative viewport")?)
            },
            name,
        )?;
    }
    let top = terminal.screen.view()?.0;
    terminal.mouse(65, (5, 4), false)?;
    terminal.wait(|t| Ok(t.screen.view()?.0 > top), "wheel down")?;
    terminal.mouse(64, (5, 4), false)?;
    terminal.wait(|t| Ok(t.screen.view()?.0 == top), "wheel up")?;
    terminal.send(b"\x1b[6~")?;
    terminal.wait(|t| Ok(t.screen.view()?.0 > top + 1), "Page Down")?;
    terminal.send(b"\x1b[5~")?;
    terminal.wait(|t| Ok(t.screen.view()?.0 == top), "Page Up")?;

    // Reconstruct every row. Each node in this linear stack must stay in column
    // zero, including nodes far below the initial viewport.
    let mut seen = BTreeMap::new();
    let count = loop {
        let (first, last, count) = terminal.screen.view()?;
        seen.extend((first - 1..last).zip(terminal.screen.body()));
        if last == count {
            break count;
        }
        terminal.send(b"\x1b[6~")?;
        terminal.wait(|t| Ok(t.screen.view()?.0 > first), "next graph page")?;
    };
    ensure!(
        seen.keys().copied().eq(0..count),
        "Paging skipped graph rows: {seen:?}"
    );
    let lines: Vec<_> = seen.into_values().collect();
    ensure!(
        lines.len() == 25,
        "Expected eight PRs, three lines each, one base: {lines:?}"
    );
    for (index, number) in (16..=23).rev().enumerate() {
        let metadata = &lines[index * 3];
        let title = &lines[index * 3 + 1];
        let url = &lines[index * 3 + 2];
        let marker = if number == 17 { "@" } else { "○" };
        let branch = match number {
            17 => "topic".into(),
            16 => "main".into(),
            _ => format!("topic-{number}"),
        };
        let state = if number == 17 { "draft" } else { "ready" };
        ensure!(
            metadata.starts_with(&format!("{marker} #{number} {branch} [{state}]")),
            "{metadata}"
        );
        let expected_title = if number == 17 {
            "Offline review fixture".into()
        } else {
            format!("Stack level {number}")
        };
        ensure!(*title == format!("│ {expected_title}"), "{title}");
        ensure!(
            *url == format!("│ https://github.com/acme/widgets/pull/{number}"),
            "{url}"
        );
    }
    ensure!(
        lines.last().map(String::as_str) == Some("◆ trunk (base)"),
        "{lines:?}"
    );
    ensure!(
        lines.iter().filter(|line| line.contains("(base)")).count() == 1,
        "{lines:?}"
    );
    terminal.send(b"\x1b[H")?;
    terminal.wait(|t| Ok(t.screen.view()?.0 == 1), "Home after paging")?;
    terminal.send(b"\x1b[F")?;
    terminal.wait(|t| Ok(t.screen.view()?.1 == count), "End")?;
    terminal.send(b"\x1b[H")?;
    terminal.wait(
        |t| Ok(!current_visible(&t.screen)),
        "Home before revealing @",
    )?;
    terminal.send(b"@")?;
    terminal.wait(
        |t| Ok(current_visible(&t.screen)),
        "@ to reveal the current PR",
    )
}

fn refresh(binary: &OsString, terminal: &mut Terminal, gate: &ListingGate) -> Result<()> {
    let original = terminal.screen.body();
    let old_count = listing_count()?;
    gate.pause()?;
    configure(json!({"title": "Manually refreshed PR title", "draft": false}))?;
    let result = (|| -> Result<()> {
        terminal.send(b"r")?;
        terminal.wait(
            |_| Ok(gate.started.exists()),
            "manual refresh to reach the paused offline listing",
        )?;
        ensure!(
            terminal.screen.body() == original,
            "A pending refresh discarded the old graph"
        );
        other_review_command(binary, terminal)?;
        terminal.send(b"\x1b[H")?;
        terminal.wait(
            |t| Ok(t.screen.view()?.0 == 1),
            "scroll during a paused read",
        )?;
        terminal.send(b"@")?;
        terminal.wait(
            |t| Ok(current_visible(&t.screen)),
            "reveal @ during a paused read",
        )
    })();
    gate.release()?;
    result?;
    terminal.wait(
        |t| {
            Ok(current_updated(
                &t.screen,
                "Manually refreshed PR title",
                false,
            ))
        },
        "r to show the changed title and draft state",
    )?;
    ensure!(
        listing_count()? > old_count,
        "Manual refresh did not list PRs"
    );

    let graph = terminal.screen.body();
    let viewport = terminal.screen.viewport();
    configure(json!({"deny_discovery": true}))?;
    terminal.send(b"r")?;
    terminal.wait(
        |t| Ok(t.screen.text().contains("Refresh failed")),
        "failed GitHub read to appear in the status",
    )?;
    terminal.keep(
        |t| Ok(t.screen.body() == graph && t.screen.viewport() == viewport),
        Duration::from_millis(300),
        "A failed read discarded the graph or moved the viewport",
    )?;
    ensure!(
        calls()?
            .iter()
            .any(|call| call["discovery_denied"].as_bool() == Some(true)),
        "No failed read occurred"
    );
    configure(json!({"deny_discovery": false, "title": "Recovered PR title", "draft": true}))?;
    terminal.send(b"r")?;
    terminal.wait(
        |t| {
            Ok(current_updated(&t.screen, "Recovered PR title", true)
                && !t.screen.text().contains("Refresh failed"))
        },
        "r to recover after the failed read",
    )?;
    terminal.exit_and_assert_restored(b"q")
}

fn queued_refresh(terminal: &mut Terminal, gate: &ListingGate) -> Result<()> {
    let old_count = listing_count()?;
    configure(json!({"title": "Queued refresh title", "draft": false}))?;
    gate.pause()?;
    let result = (|| -> Result<()> {
        terminal.send(b"r")?;
        terminal.wait(
            |_| Ok(gate.started.exists()),
            "refresh to reach the paused listing",
        )?;
        // Home's redraw proves the preceding keys were processed while blocked.
        terminal.send(b"rrr\x1b[H")?;
        terminal.wait(|t| Ok(t.screen.view()?.0 == 1), "queued r keys before Home")?;
        terminal.send(b"@")?;
        terminal.wait(
            |t| Ok(current_visible(&t.screen)),
            "reveal current during the paused read",
        )?;
        ensure!(
            listing_count()? == old_count,
            "Paused listing reached the offline transport"
        );
        Ok(())
    })();
    gate.release()?;
    result?;
    terminal.wait(
        |t| {
            Ok(current_updated(&t.screen, "Queued refresh title", false)
                && t.screen.lines()[0].contains(" · live · ")
                && listing_count()? == old_count + 2)
        },
        "one completed follow-up read for the queued refresh keys",
    )?;
    terminal.keep(
        |_| Ok(listing_count()? == old_count + 2),
        Duration::from_millis(500),
        "Queued r presses started more than one follow-up read",
    )?;
    terminal.exit_and_assert_restored(b"q")
}

fn automatic_refresh(terminal: &mut Terminal) -> Result<()> {
    let viewport = terminal.screen.viewport();
    let old_count = listing_count()?;
    let started = Instant::now();
    configure(json!({"title": "Automatically refreshed PR title", "draft": false}))?;
    // Send no input: both changed fields must come from the periodic read.
    terminal.wait_for(
        |t| {
            Ok(current_updated(
                &t.screen,
                "Automatically refreshed PR title",
                false,
            ))
        },
        "30-second automatic refresh without r",
        Duration::from_secs(36),
    )?;
    ensure!(
        started.elapsed() >= Duration::from_secs(25),
        "Automatic refresh ran too early: {:?}",
        started.elapsed()
    );
    ensure!(
        listing_count()? == old_count + 1,
        "Automatic refresh did not make exactly one read"
    );
    ensure!(
        terminal.screen.viewport() == viewport,
        "Automatic refresh moved the viewport"
    );
    ensure!(
        !terminal.screen.text().contains("Refresh failed"),
        "Automatic refresh failed"
    );
    terminal.assert_active()?;
    terminal.exit_and_assert_restored(b"q")
}

fn follow_active_review(
    binary: &OsString,
    terminal: &mut Terminal,
    gate: Option<&ListingGate>,
) -> Result<()> {
    let control = env_path("ZPR_TEST_CONTROL")?;
    let root = control.parent().context("Control has no parent")?;
    let active = env_path("ZPR_TEST_STATE")?.join("active.json");
    let original = fs::read(&active)?;
    let alternate = fs::read(root.join("stack-next-active.json"))?;
    let pages = if let Some(gate) = gate {
        terminal.wait(
            |_| Ok(gate.started.exists()),
            "initial discovery after origin pins and checkout validation",
        )?;
        terminal.assert_active()?;
        ensure!(
            terminal.screen.lines()[0].contains("PR stack"),
            "No PR stack header"
        );
        ensure!(
            !current_visible(&terminal.screen),
            "Initial discovery completed before the switch"
        );
        let control: Value = serde_json::from_slice(&fs::read(&control)?)?;
        Some(control["pulls_pages"].clone())
    } else {
        None
    };
    atomic_write(&active, &alternate)?;
    let result = (|| -> Result<()> {
        if let Some(gate) = gate {
            // Fail the original discovery after origin validation. Both sessions
            // use the same commits, so this race requires no checkout writes.
            configure(json!({"pulls_pages": [[24]]}))?;
            gate.release()?;
            terminal.wait(
                |t| {
                    Ok(t.screen
                        .text()
                        .contains("Refresh failed: Current PR is absent"))
                },
                "initial discovery failure after the active review changed",
            )?;
            ensure!(
                !current_visible(&terminal.screen),
                "Failed initial discovery displayed the old PR"
            );
            ensure!(
                listing_count()? == 1,
                "Initial discovery did not make exactly one listing"
            );
            configure(json!({"pulls_pages": pages.as_ref().context("Missing original pages")?}))?;
        }
        terminal.send(b"r")?;
        terminal.wait(
            |t| {
                Ok(t.screen
                    .body()
                    .contains(&"@ #24 alternate-topic [ready] (current)".into())
                    && t.screen.body().contains(&"│ New active review".into()))
            },
            "viewer to follow the changed active PR",
        )?;
        ensure!(
            terminal
                .screen
                .body()
                .contains(&"◆ alternate-main (base)".into()),
            "Missing alternate base"
        );
        ensure!(
            !current_visible(&terminal.screen),
            "Old review remained current"
        );
        ensure!(
            fs::read(&active)? == alternate,
            "Viewer replaced the new active session"
        );
        let result = pty::output(Command::new(binary).arg("stack"), Duration::from_secs(8))?;
        ensure!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        ensure!(stdout.contains("@ #17 topic [draft]"), "{stdout}");
        ensure!(!stdout.contains("@ #24"), "{stdout}");
        ensure!(
            fs::read(&active)? == alternate,
            "Original diff command changed the active session"
        );
        terminal.keep(
            |t| {
                Ok(t.screen
                    .body()
                    .iter()
                    .any(|line| line.starts_with("@ #24 ")))
            },
            Duration::from_millis(200),
            "Original diff command rebound the running viewer",
        )
    })();
    // Restore only the pointer and fake listing. The integration checkpoint
    // verifies checkout, drafts, refs, and every transport invocation.
    let cleanup = (|| -> Result<()> {
        if let Some(gate) = gate {
            gate.release()?;
            configure(json!({"pulls_pages": pages.context("Missing original pages")?}))?;
        }
        Ok(())
    })();
    let restored = atomic_write(&active, &original);
    result?;
    cleanup?;
    restored?;
    terminal.send(b"r")?;
    terminal.wait(
        |t| Ok(current_updated(&t.screen, "Offline review fixture", true)),
        "viewer to follow the restored active PR",
    )?;
    terminal.exit_and_assert_restored(b"q")
}

fn stdout_only_tty(binary: &OsString) -> Result<()> {
    let mut terminal = Terminal::open(
        Command::new(binary).args(["stack", "--watch"]),
        40,
        120,
        false,
    )?;
    let result = (|| -> Result<()> {
        terminal.wait_for(
            |t| Ok(t.child.0.try_wait()?.is_some()),
            "stdout-only TTY one-shot read",
            Duration::from_secs(8),
        )?;
        terminal.drain()?;
        ensure!(
            terminal
                .child
                .0
                .try_wait()?
                .context("One-shot viewer did not exit")?
                .success(),
            "One-shot viewer failed"
        );
        let text = terminal.screen.text();
        ensure!(
            text.contains("PR stack") && text.contains("@ #17 topic [draft]"),
            "{text}"
        );
        ensure!(
            text.contains("○ #23 topic-23") && text.contains("◆ trunk (base)"),
            "{text}"
        );
        ensure!(text.matches("(base)").count() == 1, "{text}");
        ensure!(
            !terminal
                .output
                .windows(b"\x1b[?1049h".len())
                .any(|part| part == b"\x1b[?1049h"),
            "Fallback entered the alternate screen"
        );
        ensure!(
            !MOUSE_MODES
                .iter()
                .any(|mode| terminal.screen.mode(*mode) == Some(true)),
            "Fallback enabled mouse reporting"
        );
        terminal
            .pty
            .assert_restored()
            .context("Fallback changed terminal attributes")
    })();
    result.with_context(|| terminal.transcript())
}

pub fn run(args: Vec<OsString>) -> Result<i32> {
    // The original CLI drivers receive binary/scenario; accept an explicit root
    // as well, without changing the CLI fixture's cwd or isolated environment.
    let (binary, scenario) = match args.as_slice() {
        [binary, scenario] | [binary, _, scenario] => (binary, scenario),
        _ => anyhow::bail!("stack-terminal expects binary, [fixture root,] scenario"),
    };
    let scenario = scenario.to_str().context("Invalid scenario")?;
    ensure!(
        matches!(
            scenario,
            "stdin-pipe"
                | "initial-active-review"
                | "navigation"
                | "queued-refresh"
                | "automatic-refresh"
                | "active-review"
                | "ctrl-c"
                | "escape"
        ),
        "Unknown stack scenario {scenario}"
    );
    pty::session(Duration::from_secs(45))?;
    if scenario == "stdin-pipe" {
        stdout_only_tty(binary)?;
    } else {
        let gate = if matches!(
            scenario,
            "initial-active-review" | "navigation" | "queued-refresh"
        ) {
            Some(ListingGate::install()?)
        } else {
            None
        };
        if scenario == "initial-active-review" {
            gate.as_ref().context("Missing initial gate")?.pause()?;
        }
        let mut terminal = Terminal::open(
            Command::new(binary).args(["stack", "--watch"]),
            10,
            80,
            true,
        )?;
        let result = (|| -> Result<()> {
            if scenario == "initial-active-review" {
                return follow_active_review(binary, &mut terminal, gate.as_ref());
            }
            terminal.wait(
                |t| Ok(current_updated(&t.screen, "Offline review fixture", true)),
                "initial @ #17 below the first page",
            )?;
            ensure!(
                terminal.screen.lines()[0].contains("PR stack"),
                "No PR stack header"
            );
            ensure!(
                terminal.screen.view()?.0 > 1,
                "Fixture did not put @ below the first page"
            );
            terminal.assert_active()?;
            terminal.screen.assert_colored_graph()?;
            let previous = calls()?;
            terminal.keep(
                |t| Ok(current_visible(&t.screen)),
                Duration::from_millis(1200),
                "Idle viewer lost the current PR",
            )?;
            ensure!(
                calls()? == previous,
                "Idle viewer polled GitHub before 30 seconds"
            );
            match scenario {
                "navigation" => {
                    other_review_command(binary, &mut terminal)?;
                    navigation(&mut terminal)?;
                    refresh(
                        binary,
                        &mut terminal,
                        gate.as_ref().context("Missing refresh gate")?,
                    )
                }
                "queued-refresh" => {
                    queued_refresh(&mut terminal, gate.as_ref().context("Missing queued gate")?)
                }
                "automatic-refresh" => automatic_refresh(&mut terminal),
                "active-review" => follow_active_review(binary, &mut terminal, None),
                "ctrl-c" => terminal.exit_and_assert_restored(b"\x03"),
                "escape" => terminal.exit_and_assert_restored(b"\x1b"),
                _ => unreachable!(),
            }
        })();
        result.with_context(|| terminal.transcript())?;
    }
    println!("PR stack PTY scenario {scenario} passed");
    Ok(0)
}
