#![cfg(unix)]

//! Real terminal input/output with isolated jj repositories and no GUI or network.

use anyhow::{Context, Result, ensure};
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn run_terminal_test(scenario: &str) -> Result<()> {
    let temp = tempfile::Builder::new()
        .prefix("zpr-log-terminal-")
        .tempdir()?;
    let root = temp.path().canonicalize()?;
    let transcript = root.join("test-output.txt");
    let output = File::create(&transcript)?;
    let mut child = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/log_terminal.py"))
        .arg(env!("CARGO_BIN_EXE_zed-pr-review"))
        .arg(&root)
        .arg(scenario)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").context("Missing PATH")?)
        .env("HOME", &root)
        .env("PYTHONUTF8", "1")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .spawn()
        .context("Python 3 is required for the PTY tests")?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // Python starts a session containing the viewer and its jj children. Also
    // clean up that process group if Python times out or exits during a failure.
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
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
