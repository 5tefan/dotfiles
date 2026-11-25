#![cfg(unix)]

//! Real terminal input/output with isolated jj repositories and no GUI or network.

use anyhow::{Context, Result, ensure};
use std::fs::{self, File};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Driver(Child);

impl Drop for Driver {
    fn drop(&mut self) {
        // The helper creates a session containing the viewer and all of its jj
        // children. Clean up the group on success, assertion failure, or timeout.
        // SAFETY: this PID came from our Child; the negative value targets only
        // its process group, never the test runner's group.
        unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match self.0.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                    Err(error) => {
                        eprintln!("Cannot reap PTY driver: {error}");
                        return;
                    }
                }
            }
            eprintln!("PTY driver did not exit within five seconds of SIGKILL");
        }
    }
}

fn run_terminal_test(scenario: &str) -> Result<()> {
    let temp = tempfile::Builder::new()
        .prefix("zpr-log-terminal-")
        .tempdir()?;
    let root = temp.path().canonicalize()?;
    let transcript = root.join("test-output.txt");
    let output = File::create(&transcript)?;
    let mut child = Driver(
        Command::new(env!("CARGO_BIN_EXE_zpr-test-helper"))
            .arg("log-terminal")
            .arg(env!("CARGO_BIN_EXE_zed-pr-review"))
            .arg(&root)
            .arg(scenario)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").context("Missing PATH")?)
            .env("HOME", &root)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()
            .context("Cannot start the native log PTY driver")?,
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(25));
    };
    drop(child);
    let output = fs::read_to_string(&transcript)?;
    ensure!(
        status.is_some(),
        "PTY test {scenario} exceeded 90 seconds:\n{output}"
    );
    ensure!(
        status.unwrap().success(),
        "PTY test {scenario} failed:\n{output}"
    );
    Ok(())
}

#[test]
fn log_terminal_navigation_and_quit_restore_the_terminal() -> Result<()> {
    run_terminal_test("navigation")
}

#[test]
fn log_terminal_ctrl_c_restores_the_terminal() -> Result<()> {
    run_terminal_test("ctrl-c")
}
