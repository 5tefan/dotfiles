//! Best-effort preparation of adjacent reviews, without changing jj's checkout.

use crate::github::Client;
use crate::model::PullRequest;
use crate::session::Session;
use crate::stack::Stack;
use crate::store::{Store, draft_key};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Start a detached, finite worker after the current diff or graph is available.
/// Its output must not inherit the task terminal or keep its pipes open.
pub fn start(client: &Client, pr: &PullRequest, state: &Path) {
    if std::env::var_os("ZED_PR_REVIEW_PREFETCH").is_some_and(|value| value == "0")
        || running(state)
    {
        return;
    }
    let result = (|| -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .current_dir(&client.root)
            .args(["--directory"])
            .arg(&client.root)
            .args(["--pr", &pr.url, "prefetch"])
            .env_remove("ZED_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().context("Cannot start stack preparation")?;
        // Reap while the caller stays alive (the live graph); short-lived callers
        // may exit first, in which case the OS adopts the detached child.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Background preparation unavailable: {error:#}");
    }
}

fn running(state: &Path) -> bool {
    let Ok(file) = File::open(state.join("prefetch/lock")) else {
        return false;
    };
    file.try_lock().is_err()
}

/// Foreground work waits only if a background worker is finishing this PR.
/// Fetches happen before the per-PR lock, so they never hold up an open/comment.
pub fn open_store(state: &Path, pr: &PullRequest) -> Result<Store> {
    let started = Instant::now();
    let mut announced = false;
    loop {
        match Store::open(state, pr) {
            Ok(store) => return Ok(store),
            Err(error) => {
                let preparing = fs::read(state.join("prefetch/status.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .is_some_and(|status| {
                        matches!(status["state"].as_str(), Some("checking" | "preparing"))
                            && status["draft_key"] == draft_key(pr)
                    });
                if !preparing || !running(state) || started.elapsed() >= Duration::from_secs(30) {
                    return Err(error);
                }
                if !announced {
                    println!("Finishing background preparation for PR #{}…", pr.number);
                    announced = true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

pub fn run(client: &Client, current: &PullRequest, state: &Path) -> Result<()> {
    ensure!(
        state == client.root.join("scratch/zed-pr-review") && state.is_dir(),
        "Open a PR before preparing its stack"
    );
    let worker = match Store::at(&state.join("prefetch")) {
        Ok(worker) => worker,
        Err(_) if running(state) => return Ok(()),
        Err(error) => return Err(error),
    };
    let result = (|| -> Result<()> {
        worker.write_json("status.json", &json!({"state": "discovering"}))?;
        let stack = match Stack::cached(state, current) {
            Some(stack) => stack,
            None => {
                let stack = Stack::discover(client, current)?;
                stack.remember(state)?;
                stack
            }
        };
        let entries = stack.preparation_order();
        let mut ready = 0;
        let mut errors = Vec::new();
        for entry in &entries {
            worker.write_json(
                "status.json",
                &json!({
                    "state": "checking", "pr": entry.pr.number,
                    "draft_key": draft_key(&entry.pr), "ready": ready, "total": entries.len(),
                }),
            )?;
            let prepared =
                (|| -> Result<bool> {
                    // Probe under the draft lock, but never wait for a foreground review.
                    let store = match Store::open(state, &entry.pr) {
                        Ok(store) => store,
                        Err(_) => return Ok(false),
                    };
                    if Session::prepared(&store, &entry.pr)?.is_some() {
                        return Ok(true);
                    }
                    drop(store);
                    worker.write_json("status.json", &json!({
                        "state": "fetching", "pr": entry.pr.number,
                        "draft_key": draft_key(&entry.pr), "ready": ready, "total": entries.len(),
                    }))?;
                    client.prefetch_pr(&entry.pr, &entry.base_ref)?;
                    let files = client.files(entry.pr.number)?;
                    ensure!(
                        entry.pr.same_revision(&client.read_pr(entry.pr.number)?),
                        "PR changed while fetching background review data"
                    );
                    worker.write_json("status.json", &json!({
                    "state": "preparing", "pr": entry.pr.number,
                    "draft_key": draft_key(&entry.pr), "ready": ready, "total": entries.len(),
                }))?;
                    let store = match Store::open(state, &entry.pr) {
                        Ok(store) => store,
                        Err(_) => return Ok(false),
                    };
                    if Session::prepared(&store, &entry.pr)?.is_none() {
                        let session = Session::prepare_local(client, &store, &entry.pr, &files)?;
                        session.remember(&store)?;
                    }
                    Ok(true)
                })();
            match prepared {
                Ok(true) => ready += 1,
                Ok(false) => {} // A foreground review has priority; retry on a later refresh.
                Err(error) => errors.push(format!("PR #{}: {error:#}", entry.pr.number)),
            }
        }
        worker.write_json(
            "status.json",
            &json!({
                "state": "complete", "ready": ready, "total": entries.len(), "errors": errors,
            }),
        )?;
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = worker.write_json(
            "status.json",
            &json!({
                "state": "failed", "error": format!("{error:#}"),
            }),
        );
    }
    result
}

pub fn status(state: &Path) -> Option<String> {
    let bytes = fs::read(state.join("prefetch/status.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let ready = value["ready"].as_u64().unwrap_or(0);
    let total = value["total"].as_u64().unwrap_or(0);
    Some(match value["state"].as_str()? {
        "discovering" => "Preparing stack: discovering…".into(),
        "checking" | "fetching" | "preparing" => format!(
            "Prepared {ready}/{total} adjacent PRs · preparing #{}",
            value["pr"].as_u64()?
        ),
        "complete" => {
            let errors = value["errors"].as_array().map_or(0, Vec::len);
            if errors == 0 {
                format!("Prepared {ready}/{total} adjacent PRs")
            } else {
                format!(
                    "Prepared {ready}/{total} adjacent PRs · {errors} failed (prefetch/status.json)"
                )
            }
        }
        "failed" => "Stack preparation failed (prefetch/status.json)".into(),
        _ => return None,
    })
}
