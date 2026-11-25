//! Finish-menu choices, dismissal, editor handoff, and post-menu signals.
#![cfg(unix)]

use super::pty::{self, Pty, Terminal};
use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn editor_marker() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("ZPR_TEST_EDITOR_LOG").context("Missing ZPR_TEST_EDITOR_LOG")?,
    )
    .with_file_name("review-terminal-editor-restored"))
}

/// Called by the native editor before any draft reads, writes, or logging. The
/// normal fixture editor is unchanged unless the PTY driver sets this variable.
pub fn editor_handoff() -> Result<()> {
    let Some(expected) = std::env::var_os("ZPR_REVIEW_PTY_TERMIOS") else {
        return Ok(());
    };
    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let expected: serde_json::Value =
        serde_json::from_str(expected.to_str().context("Invalid termios JSON")?)?;
    ensure!(
        pty::terminal_attributes(tty.as_raw_fd())? == expected,
        "The summary editor started before the terminal was restored"
    );
    drop(tty);
    fs::write(editor_marker()?, "restored before editor\n")?;
    if let Some(descriptor) = std::env::var_os("ZPR_REVIEW_PTY_EDITOR_PIPE") {
        let descriptor: i32 = descriptor
            .to_str()
            .context("Invalid editor pipe")?
            .parse()?;
        ensure!(
            descriptor > 2,
            "Editor pipe must not alias standard input/output"
        );
        // SAFETY: this descriptor was explicitly inherited from this driver's
        // pipe. The editor takes its sole ownership and holds it until exit.
        let mut writer = unsafe { File::from_raw_fd(descriptor) };
        writeln!(writer, "{}", std::process::id())?;
        // EOF remains observable after the CLI dies and this editor is orphaned.
        // The supervisor normally kills it; this deadline also bounds failures
        // when the helper is invoked without the integration watchdog.
        std::thread::sleep(Duration::from_secs(20));
        drop(writer);
        bail!("The paused review editor was not killed within 20 seconds");
    }
    Ok(())
}

struct EditorPipe {
    reader: File,
    writer: File,
}

impl EditorPipe {
    fn open() -> Result<Self> {
        let mut descriptors = [-1; 2];
        // SAFETY: pipe initializes two descriptors on success. Both are owned
        // immediately, so subsequent fcntl/spawn failures close them via RAII.
        ensure!(
            unsafe { libc::pipe(descriptors.as_mut_ptr()) } == 0,
            "pipe: {}",
            std::io::Error::last_os_error()
        );
        let result = Self {
            reader: unsafe { File::from_raw_fd(descriptors[0]) },
            writer: unsafe { File::from_raw_fd(descriptors[1]) },
        };
        for file in [&result.reader, &result.writer] {
            ensure!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "fcntl(FD_CLOEXEC): {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(result)
    }
}

struct PausedEditor {
    pipe: File,
    pid: Option<libc::pid_t>,
}

impl PausedEditor {
    fn receive_pid(&mut self, cli_pid: u32) -> Result<libc::pid_t> {
        ensure!(
            pty::readable(self.pipe.as_raw_fd(), Duration::from_secs(5))?,
            "Editor did not report its PID"
        );
        let mut buffer = [0; 64];
        let size = self.pipe.read(&mut buffer)?;
        let pid: libc::pid_t = std::str::from_utf8(&buffer[..size])?.trim().parse()?;
        ensure!(
            pid > 1 && pid != std::process::id() as libc::pid_t && pid != cli_pid as libc::pid_t,
            "Invalid paused editor PID {pid}"
        );
        self.pid = Some(pid);
        Ok(pid)
    }

    fn kill_and_wait(&mut self) -> Result<()> {
        let Some(pid) = self.pid else {
            return Ok(());
        };
        // The editor may have become an orphan; wait for its held pipe to close
        // rather than using waitpid on a process that is not always our child.
        let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
        if killed != 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.raw_os_error() == Some(libc::ESRCH),
                "kill paused editor: {error}"
            );
        }
        ensure!(
            pty::readable(self.pipe.as_raw_fd(), Duration::from_secs(5))?,
            "Paused editor did not exit"
        );
        let mut buffer = [0; 64];
        ensure!(
            self.pipe.read(&mut buffer)? == 0,
            "Editor exit pipe did not close"
        );
        self.pid = None;
        Ok(())
    }
}

impl Drop for PausedEditor {
    fn drop(&mut self) {
        if let Err(error) = self.kill_and_wait() {
            eprintln!("Paused editor cleanup failed: {error:#}");
        }
    }
}

fn terminal(binary: &OsString, pause_editor: bool) -> Result<(Terminal, Option<PausedEditor>)> {
    let pty = Pty::open(24, 100, true)?;
    let mut command = Command::new(binary);
    command.arg("finish").env(
        "ZPR_REVIEW_PTY_TERMIOS",
        serde_json::to_string(&pty::attributes_json(&pty.before))?,
    );
    // EDITOR remains the native fixture editor, preserving all of its arguments
    // and behavior. Its editor_handoff hook runs before the ordinary editor.
    let pipe = if pause_editor {
        Some(EditorPipe::open()?)
    } else {
        None
    };
    if let Some(pipe) = &pipe {
        let writer = pipe.writer.as_raw_fd();
        command.env("ZPR_REVIEW_PTY_EDITOR_PIPE", writer.to_string());
        // SAFETY: pre_exec only performs async-signal-safe libc calls. Restore
        // defaults that the supervising driver deliberately does not inherit.
        unsafe {
            command.pre_exec(move || {
                for signum in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                    if libc::signal(signum, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::fcntl(writer, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let terminal = Terminal::spawn(&mut command, pty, true)?;
    let editor = pipe.map(|pipe| {
        drop(pipe.writer);
        PausedEditor {
            pipe: pipe.reader,
            pid: None,
        }
    });
    Ok((terminal, editor))
}

fn interrupt_editor(
    terminal: &mut Terminal,
    editor: &mut PausedEditor,
    signum: libc::c_int,
) -> Result<()> {
    let marker = editor_marker()?;
    let mouse_modes = terminal.screen.mouse_modes();
    let result = (|| -> Result<()> {
        terminal.send(b"2")?;
        terminal.wait(
            |t| Ok(marker.exists() && t.screen.mode(1049) == Some(false)),
            "paused summary editor after terminal restoration",
        )?;
        let pid = editor.receive_pid(terminal.child.0.id())?;
        terminal.assert_restored(&mouse_modes)?;
        ensure!(
            terminal.child.0.try_wait()?.is_none(),
            "CLI exited before the signal"
        );
        // Signal only the CLI after the menu ends. A swallowed signal must fail
        // within five seconds, while the editor must remain alive independently.
        ensure!(
            unsafe { libc::kill(terminal.child.0.id() as libc::pid_t, signum) } == 0,
            "Signal CLI: {}",
            std::io::Error::last_os_error()
        );
        let status = terminal
            .child
            .wait(Duration::from_secs(5))
            .with_context(|| format!("CLI ignored signal {signum} while the editor was paused"))?;
        ensure!(
            status.signal() == Some(signum),
            "Expected termination by {signum}, got {status}"
        );
        ensure!(
            unsafe { libc::kill(pid, 0) } == 0,
            "The CLI alone should have received the signal"
        );
        terminal.drain()?;
        terminal.assert_restored(&mouse_modes)
    })();
    // Close the CLI's duplicate of the pipe before checking the editor's EOF.
    let cleanup = (|| -> Result<()> {
        if terminal.child.0.try_wait()?.is_none() {
            terminal.child.0.kill()?;
        }
        terminal.child.wait(Duration::from_secs(5))?;
        editor.kill_and_wait()
    })();
    result?;
    cleanup
}

pub fn run(args: Vec<OsString>) -> Result<i32> {
    let (binary, scenario) = match args.as_slice() {
        [binary, scenario] | [binary, _, scenario] => (binary, scenario),
        _ => bail!("review-terminal expects binary, [fixture root,] scenario"),
    };
    let scenario = scenario.to_str().context("Invalid scenario")?;
    let signum = match scenario {
        "editor-sigint" => Some(libc::SIGINT),
        "editor-sigterm" => Some(libc::SIGTERM),
        "editor-sighup" => Some(libc::SIGHUP),
        _ => None,
    };
    ensure!(
        signum.is_some()
            || matches!(
                scenario,
                "quit"
                    | "escape"
                    | "ctrl-c"
                    | "enter"
                    | "invalid"
                    | "1"
                    | "2"
                    | "3"
                    | "4"
                    | "down-comment"
                    | "up-request"
            ),
        "Unknown review scenario {scenario}"
    );
    pty::session(Duration::from_secs(35))?;
    let (mut terminal, mut editor) = terminal(binary, signum.is_some())?;
    let result = (|| -> Result<()> {
        let choices = [
            "1  Exit + abandon",
            "2  Comment",
            "3  Approve",
            "4  Request changes",
        ];
        terminal.wait(
            |t| {
                let text = t.screen.text();
                Ok(choices.iter().all(|choice| text.contains(choice)))
            },
            "all four finish choices",
        )?;
        let text = terminal.screen.text();
        ensure!(
            text.contains("#17") && text.contains("1 pending comment"),
            "{text}"
        );
        ensure!(
            !text.contains('›'),
            "Opening the menu must not select an action"
        );
        terminal.assert_active()?;
        if let Some(signum) = signum {
            interrupt_editor(
                &mut terminal,
                editor.as_mut().context("Missing editor pipe")?,
                signum,
            )?;
        } else {
            match scenario {
                "quit" => terminal.exit_and_assert_restored(b"q")?,
                "escape" => terminal.exit_and_assert_restored(b"\x1b")?,
                "ctrl-c" => terminal.exit_and_assert_restored(b"\x03")?,
                "enter" => terminal.exit_and_assert_restored(b"\r")?,
                "invalid" => {
                    terminal.send(b"0x5")?;
                    terminal.keep(
                        |t| Ok(!t.screen.text().contains('›')),
                        Duration::from_millis(200),
                        "Invalid keys selected an action",
                    )?;
                    terminal.exit_and_assert_restored(b"q")?;
                }
                // Numbers act immediately: do not send Enter to hide regressions.
                "1" | "2" | "3" | "4" => terminal.exit_and_assert_restored(scenario.as_bytes())?,
                "down-comment" | "up-request" => {
                    let (choice, keys): (&str, &[u8]) = if scenario == "down-comment" {
                        ("2  Comment", b"\x1b[B\x1b[B")
                    } else {
                        ("4  Request changes", b"\x1b[A")
                    };
                    terminal.send(keys)?;
                    terminal.wait(
                        |t| {
                            Ok(t.screen
                                .lines()
                                .iter()
                                .any(|line| line.contains('›') && line.contains(choice)))
                        },
                        "arrow selection",
                    )?;
                    terminal.exit_and_assert_restored(b"\r")?;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    })();
    result.with_context(|| terminal.transcript())?;
    drop(editor);
    drop(terminal);
    let should_edit =
        signum.is_some() || matches!(scenario, "2" | "3" | "4" | "down-comment" | "up-request");
    ensure!(
        editor_marker()?.exists() == should_edit,
        "Unexpected editor handoff"
    );
    println!("Review PTY scenario {scenario} passed");
    Ok(0)
}
