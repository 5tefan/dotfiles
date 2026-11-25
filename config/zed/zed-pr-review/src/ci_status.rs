//! Asynchronous GitHub CI status polling for `zed-pr-review stack --watch` and
//! automatic `.workspaces/<name>/scratch/CI_ISSUES.md` sync for isolated agents.
//!
//! All GitHub requests run on a dedicated background thread under
//! `github::with_background_deadlines`. Network outages, timeouts, missing `gh`
//! auth, rate limits, malformed responses, and local I/O failures are swallowed
//! so the interactive `jj l` TUI is never stalled or disrupted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::NamedTempFile;

use crate::github::{self, Client};

const POLL_INTERVAL: Duration = Duration::from_secs(25);
const MAX_COMMITS_PER_QUERY: usize = 25;
const MAX_LOG_LINES: usize = 200;
const MAX_LOG_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiBadge {
    Passing,
    OptionalFailed,
    RequiredFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Passing,
    Failing,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckInfo {
    pub name: String,
    pub outcome: CheckOutcome,
    /// `Some(true)` = required, `Some(false)` = optional, `None` = unknown.
    pub is_required: Option<bool>,
    pub job_id: Option<u64>,
    pub details_url: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitCiState {
    pub commit_sha: String,
    pub pr_number: Option<u64>,
    pub checks: Vec<CheckInfo>,
    pub badge: Option<CiBadge>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilteredLog {
    pub command: Option<String>,
    pub output: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailingCheckReport {
    pub name: String,
    pub date: String,
    pub run_id: String,
    pub commit_sha: String,
    pub command: String,
    pub output: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub commit_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncFingerprint {
    Passing(String),
    Failing {
        commit_sha: String,
        checks: Vec<(String, Option<u64>, Option<String>)>,
    },
}

pub enum CiRequest {
    UpdateCommits(Vec<String>),
}

pub struct CiHandle {
    pub requests: mpsc::Sender<CiRequest>,
    pub replies: mpsc::Receiver<HashMap<String, CiBadge>>,
}

/// Spawn the dedicated background CI worker. Returns `None` only if thread
/// creation fails, allowing the TUI to proceed normally without CI indicators.
pub fn spawn_worker(root: &Path) -> Option<CiHandle> {
    let root = root.to_owned();
    let (requests, jobs) = mpsc::channel();
    let (results, replies) = mpsc::channel();
    std::thread::Builder::new()
        .name("ci-status".into())
        .spawn(move || run_worker(&root, jobs, results))
        .ok()?;
    Some(CiHandle { requests, replies })
}

struct WorkerState {
    client: Option<Client>,
    visible_commits: Vec<String>,
    last_polled_pushed: BTreeSet<String>,
    badge_cache: HashMap<(String, String), CommitCiState>,
    pr_cache: HashMap<(String, String), u64>,
    synced_workspaces: HashMap<String, SyncFingerprint>,
    last_sent_badges: HashMap<String, CiBadge>,
    last_poll: Option<Instant>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            client: None,
            visible_commits: Vec::new(),
            last_polled_pushed: BTreeSet::new(),
            badge_cache: HashMap::new(),
            pr_cache: HashMap::new(),
            synced_workspaces: HashMap::new(),
            last_sent_badges: HashMap::new(),
            last_poll: None,
        }
    }
}

fn run_worker(
    root: &Path,
    jobs: mpsc::Receiver<CiRequest>,
    results: mpsc::Sender<HashMap<String, CiBadge>>,
) {
    let mut state = WorkerState::new();
    loop {
        let timeout = match state.last_poll {
            None => POLL_INTERVAL,
            Some(last) => POLL_INTERVAL.saturating_sub(last.elapsed()),
        };
        let mut commits_updated = false;
        match jobs.recv_timeout(timeout) {
            Ok(CiRequest::UpdateCommits(commits)) => {
                state.visible_commits = commits;
                commits_updated = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Coalesce bursts of `jj l` refreshes into the latest visible commit list.
        while let Ok(CiRequest::UpdateCommits(commits)) = jobs.try_recv() {
            state.visible_commits = commits;
            commits_updated = true;
        }

        if state.visible_commits.is_empty() {
            continue;
        }

        let remote_shas = match list_remote_bookmark_commits(root) {
            Ok(shas) => shas,
            Err(_) => continue,
        };
        let pushed_visible: BTreeSet<String> = state
            .visible_commits
            .iter()
            .filter(|sha| remote_shas.contains(*sha) && is_valid_sha(sha))
            .cloned()
            .collect();

        // Emit any already-cached badges immediately when visible commits change.
        if commits_updated {
            send_visible_badges(&mut state, &pushed_visible, &results);
        }

        let due = state
            .last_poll
            .is_none_or(|last| last.elapsed() >= POLL_INTERVAL);
        let pushed_changed = pushed_visible != state.last_polled_pushed;
        if !due && !pushed_changed {
            continue;
        }

        state.last_poll = Some(Instant::now());
        state.last_polled_pushed = pushed_visible.clone();

        if pushed_visible.is_empty() {
            send_visible_badges(&mut state, &pushed_visible, &results);
            continue;
        }

        if state.client.is_none() {
            state.client = github::with_background_deadlines(|| Client::for_ci(root)).ok();
        }
        let Some(client) = state.client.clone() else {
            continue;
        };

        let shas: Vec<String> = pushed_visible.iter().cloned().collect();
        let fetched = github::with_background_deadlines(|| {
            fetch_commits_ci(&client, &shas, &mut state.pr_cache)
        });
        if let Ok(states) = fetched {
            for ci_state in states {
                if let Some(pr) = ci_state.pr_number {
                    state
                        .pr_cache
                        .insert((client.repo.clone(), ci_state.commit_sha.clone()), pr);
                }
                state
                    .badge_cache
                    .insert((client.repo.clone(), ci_state.commit_sha.clone()), ci_state);
            }
            if !send_visible_badges(&mut state, &pushed_visible, &results) {
                break;
            }
            sync_all_workspaces(root, &client, &remote_shas, &mut state);
        }
    }
}

fn send_visible_badges(
    state: &mut WorkerState,
    pushed_visible: &BTreeSet<String>,
    results: &mpsc::Sender<HashMap<String, CiBadge>>,
) -> bool {
    let mut badges = HashMap::new();
    if let Some(client) = &state.client {
        for sha in pushed_visible {
            if let Some(ci) = state.badge_cache.get(&(client.repo.clone(), sha.clone()))
                && let Some(badge) = ci.badge
            {
                badges.insert(sha.clone(), badge);
            }
        }
    }
    if badges != state.last_sent_badges {
        state.last_sent_badges = badges.clone();
        if results.send(badges).is_err() {
            return false;
        }
    }
    true
}

// ────────────────── Check Classification & Aggregation ──────────────────

pub fn classify_check_run(status: &str, conclusion: Option<&str>) -> CheckOutcome {
    if !status.eq_ignore_ascii_case("COMPLETED") {
        return CheckOutcome::Pending;
    }
    match conclusion.map(|c| c.to_ascii_uppercase()).as_deref() {
        Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckOutcome::Passing,
        Some(_) => CheckOutcome::Failing,
        None => CheckOutcome::Pending,
    }
}

pub fn classify_status_context(state: &str) -> CheckOutcome {
    match state.to_ascii_uppercase().as_str() {
        "SUCCESS" => CheckOutcome::Passing,
        "FAILURE" | "ERROR" => CheckOutcome::Failing,
        _ => CheckOutcome::Pending,
    }
}

/// Aggregate check outcomes into a single TUI badge:
/// - Red `✗`: one or more required (or unknown-requiredness) checks are failing.
/// - Yellow `!`: no required checks are failing, and one or more optional checks are failing.
/// - Green `✓`: all checks are passing (none failing or pending).
/// - `None`: no checks exist, or checks are still running with no failures yet.
pub fn aggregate_badge(checks: &[CheckInfo]) -> Option<CiBadge> {
    if checks.is_empty() {
        return None;
    }
    let mut has_required_failure = false;
    let mut has_optional_failure = false;
    let mut has_pending = false;
    for check in checks {
        match check.outcome {
            CheckOutcome::Failing => {
                if check.is_required == Some(false) {
                    has_optional_failure = true;
                } else {
                    has_required_failure = true;
                }
            }
            CheckOutcome::Pending => has_pending = true,
            CheckOutcome::Passing => {}
        }
    }
    if has_required_failure {
        Some(CiBadge::RequiredFailed)
    } else if has_optional_failure {
        Some(CiBadge::OptionalFailed)
    } else if has_pending {
        None
    } else {
        Some(CiBadge::Passing)
    }
}

// ────────────────── GitHub GraphQL Polling ──────────────────

fn is_valid_sha(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn split_owner_repo(repo: &str) -> Result<(&str, &str)> {
    let mut parts = repo.split('/');
    let owner = parts.next().filter(|s| !s.is_empty());
    let name = parts.next().filter(|s| !s.is_empty());
    match (owner, name, parts.next()) {
        (Some(owner), Some(name), None) => Ok((owner, name)),
        _ => bail!("Invalid repository name: {repo}"),
    }
}

fn fetch_commits_ci(
    client: &Client,
    shas: &[String],
    pr_cache: &mut HashMap<(String, String), u64>,
) -> Result<Vec<CommitCiState>> {
    let (owner, name) = split_owner_repo(&client.repo)?;
    let mut all_states = Vec::new();

    for chunk in shas.chunks(MAX_COMMITS_PER_QUERY) {
        let query = build_rollup_query(chunk, &client.repo, pr_cache);
        let response = client.graphql(&query, &json!({ "owner": owner, "name": name }))?;
        let mut states = parse_rollup_response(&response, chunk)?;

        // For any failing commit where we just discovered a PR number and didn't
        // include `isRequired` in the first query, do a single follow-up batch.
        let need_required: Vec<(usize, String, u64)> = states
            .iter()
            .enumerate()
            .filter_map(|(idx, state)| {
                let pr = state.pr_number?;
                let has_unknown_failure = state
                    .checks
                    .iter()
                    .any(|c| c.outcome == CheckOutcome::Failing && c.is_required.is_none());
                has_unknown_failure.then(|| (idx, state.commit_sha.clone(), pr))
            })
            .collect();

        if !need_required.is_empty() {
            let req_query = build_required_query(&need_required);
            if let Ok(req_response) =
                client.graphql(&req_query, &json!({ "owner": owner, "name": name }))
            {
                apply_required_response(&req_response, &need_required, &mut states);
            }
        }

        all_states.extend(states);
    }

    Ok(all_states)
}

fn build_rollup_query(
    shas: &[String],
    repo: &str,
    pr_cache: &HashMap<(String, String), u64>,
) -> String {
    let mut query = String::from(
        "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) {",
    );
    for (idx, sha) in shas.iter().enumerate() {
        let is_req_field = pr_cache
            .get(&(repo.to_owned(), sha.clone()))
            .map(|pr| format!(" isRequired(pullRequestNumber: {pr})"))
            .unwrap_or_default();
        query.push_str(&format!(
            " c{idx}: object(oid: \"{sha}\") {{ ... on Commit {{ oid associatedPullRequests(first: 5) {{ nodes {{ number state headRefOid }} }} statusCheckRollup {{ contexts(first: 100) {{ nodes {{ __typename ... on CheckRun {{ name status conclusion detailsUrl startedAt completedAt databaseId{is_req_field} }} ... on StatusContext {{ context state targetUrl createdAt{is_req_field} }} }} }} }} }} }}"
        ));
    }
    query.push_str(" } }");
    query
}

fn build_required_query(targets: &[(usize, String, u64)]) -> String {
    let mut query = String::from(
        "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) {",
    );
    for (q_idx, (_, sha, pr)) in targets.iter().enumerate() {
        query.push_str(&format!(
            " r{q_idx}: object(oid: \"{sha}\") {{ ... on Commit {{ statusCheckRollup {{ contexts(first: 100) {{ nodes {{ __typename ... on CheckRun {{ name isRequired(pullRequestNumber: {pr}) }} ... on StatusContext {{ context isRequired(pullRequestNumber: {pr}) }} }} }} }} }} }}"
        ));
    }
    query.push_str(" } }");
    query
}

pub fn parse_rollup_response(response: &Value, shas: &[String]) -> Result<Vec<CommitCiState>> {
    let repo = response
        .get("data")
        .and_then(|d| d.get("repository"))
        .and_then(Value::as_object)
        .context("Missing repository in GraphQL response")?;

    let mut results = Vec::with_capacity(shas.len());
    for (idx, sha) in shas.iter().enumerate() {
        let alias = format!("c{idx}");
        let Some(commit) = repo.get(&alias).and_then(Value::as_object) else {
            continue;
        };
        let pr_number = select_associated_pr(commit.get("associatedPullRequests"), sha);
        let checks = parse_contexts(commit.get("statusCheckRollup"));
        let badge = aggregate_badge(&checks);
        results.push(CommitCiState {
            commit_sha: sha.clone(),
            pr_number,
            checks,
            badge,
        });
    }
    Ok(results)
}

fn select_associated_pr(associated: Option<&Value>, sha: &str) -> Option<u64> {
    let nodes = associated?.get("nodes")?.as_array()?;
    let mut open_match = None;
    let mut any_open = None;
    let mut first = None;
    for node in nodes {
        let Some(number) = node.get("number").and_then(Value::as_u64) else {
            continue;
        };
        if first.is_none() {
            first = Some(number);
        }
        let is_open = node
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("OPEN"));
        if is_open {
            if any_open.is_none() {
                any_open = Some(number);
            }
            if node
                .get("headRefOid")
                .and_then(Value::as_str)
                .is_some_and(|oid| oid.eq_ignore_ascii_case(sha))
            {
                open_match = Some(number);
            }
        }
    }
    open_match.or(any_open).or(first)
}

fn parse_contexts(rollup: Option<&Value>) -> Vec<CheckInfo> {
    let Some(nodes) = rollup
        .and_then(|r| r.get("contexts"))
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut checks = Vec::new();
    for node in nodes {
        let Some(typename) = node.get("__typename").and_then(Value::as_str) else {
            continue;
        };
        match typename {
            "CheckRun" => {
                let Some(name) = node.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let status = node.get("status").and_then(Value::as_str).unwrap_or("");
                let conclusion = node.get("conclusion").and_then(Value::as_str);
                let details_url = node
                    .get("detailsUrl")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let job_id = node
                    .get("databaseId")
                    .and_then(Value::as_u64)
                    .or_else(|| details_url.as_deref().and_then(extract_job_id_from_url));
                let timestamp = node
                    .get("completedAt")
                    .and_then(Value::as_str)
                    .or_else(|| node.get("startedAt").and_then(Value::as_str))
                    .map(str::to_owned);
                let is_required = node.get("isRequired").and_then(Value::as_bool);
                checks.push(CheckInfo {
                    name: name.to_owned(),
                    outcome: classify_check_run(status, conclusion),
                    is_required,
                    job_id,
                    details_url,
                    timestamp,
                });
            }
            "StatusContext" => {
                let Some(context) = node.get("context").and_then(Value::as_str) else {
                    continue;
                };
                let state = node.get("state").and_then(Value::as_str).unwrap_or("");
                let details_url = node
                    .get("targetUrl")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let job_id = details_url.as_deref().and_then(extract_job_id_from_url);
                let timestamp = node
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let is_required = node.get("isRequired").and_then(Value::as_bool);
                checks.push(CheckInfo {
                    name: context.to_owned(),
                    outcome: classify_status_context(state),
                    is_required,
                    job_id,
                    details_url,
                    timestamp,
                });
            }
            _ => {}
        }
    }
    checks
}

pub fn apply_required_response(
    response: &Value,
    targets: &[(usize, String, u64)],
    states: &mut [CommitCiState],
) {
    let Some(repo) = response
        .get("data")
        .and_then(|d| d.get("repository"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for (q_idx, (state_idx, _, _)) in targets.iter().enumerate() {
        let alias = format!("r{q_idx}");
        let Some(nodes) = repo
            .get(&alias)
            .and_then(|c| c.get("statusCheckRollup"))
            .and_then(|r| r.get("contexts"))
            .and_then(|c| c.get("nodes"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let mut required_map: HashMap<&str, bool> = HashMap::new();
        for node in nodes {
            let name = node
                .get("name")
                .or_else(|| node.get("context"))
                .and_then(Value::as_str);
            let req = node.get("isRequired").and_then(Value::as_bool);
            if let (Some(name), Some(req)) = (name, req) {
                required_map.insert(name, req);
            }
        }
        if let Some(state) = states.get_mut(*state_idx) {
            for check in &mut state.checks {
                if let Some(&req) = required_map.get(check.name.as_str()) {
                    check.is_required = Some(req);
                }
            }
            state.badge = aggregate_badge(&state.checks);
        }
    }
}

// ────────────────── Job Log Filtering & Markdown Formatting ──────────────────

pub fn extract_job_id_from_url(url: &str) -> Option<u64> {
    let marker = "/job/";
    let idx = url.find(marker)?;
    let rest = &url[idx + marker.len()..];
    let digits_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    rest[..digits_len].parse().ok()
}

pub fn strip_iso8601_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    // Minimum prefix: YYYY-MM-DDTHH:MM:SSZ (20 bytes)
    if bytes.len() < 20 {
        return line;
    }
    let valid_date = bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b':'
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b':'
        && bytes[17..19].iter().all(u8::is_ascii_digit);
    if !valid_date {
        return line;
    }
    let mut pos = 19;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let frac_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == frac_start {
            return line;
        }
    }
    if bytes.get(pos) != Some(&b'Z') {
        return line;
    }
    pos += 1;
    while matches!(bytes.get(pos), Some(b' ' | b'\t')) {
        pos += 1;
    }
    &line[pos..]
}

fn extract_group_command(line: &str) -> Option<String> {
    let idx = line.find("##[group]")?;
    let label = line[idx + "##[group]".len()..].trim();
    if label.is_empty() {
        return None;
    }
    let cmd = label.strip_prefix("Run ").unwrap_or(label).trim();
    (!cmd.is_empty()).then(|| cmd.to_owned())
}

pub fn filter_job_logs(raw_logs: &str) -> FilteredLog {
    let lines: Vec<&str> = raw_logs
        .lines()
        .map(|l| strip_iso8601_prefix(l.trim_end_matches('\r')))
        .collect();

    let (command, selected): (Option<String>, Vec<&str>) =
        if let Some(err_idx) = lines.iter().rposition(|l| l.contains("##[error]")) {
            if let Some(endgroup_idx) = lines[..err_idx]
                .iter()
                .rposition(|l| l.contains("##[endgroup]"))
            {
                let cmd = lines[..endgroup_idx]
                    .iter()
                    .rposition(|l| l.contains("##[group]"))
                    .and_then(|g_idx| extract_group_command(lines[g_idx]));
                let section: Vec<&str> = lines[endgroup_idx + 1..=err_idx]
                    .iter()
                    .copied()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                (cmd, section)
            } else {
                let section: Vec<&str> = lines[..=err_idx]
                    .iter()
                    .copied()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                (None, section)
            }
        } else if let Some(endgroup_idx) = lines.iter().rposition(|l| l.contains("##[endgroup]")) {
            let cmd = lines[..endgroup_idx]
                .iter()
                .rposition(|l| l.contains("##[group]"))
                .and_then(|g_idx| extract_group_command(lines[g_idx]));
            let section: Vec<&str> = lines[endgroup_idx + 1..]
                .iter()
                .copied()
                .filter(|l| !l.trim().is_empty())
                .collect();
            (cmd, section)
        } else {
            let section: Vec<&str> = lines
                .iter()
                .copied()
                .filter(|l| !l.trim().is_empty())
                .collect();
            (None, section)
        };

    let output = cap_log_output(&selected);
    FilteredLog { command, output }
}

fn cap_log_output(lines: &[&str]) -> String {
    let mut start = lines.len().saturating_sub(MAX_LOG_LINES);
    let mut joined = lines[start..].join("\n");
    while joined.len() > MAX_LOG_BYTES && start + 1 < lines.len() {
        start += 1;
        joined = lines[start..].join("\n");
    }
    if joined.len() > MAX_LOG_BYTES {
        let mut cut = joined.len() - MAX_LOG_BYTES;
        while !joined.is_char_boundary(cut) {
            cut += 1;
        }
        joined = format!("... (truncated)\n{}", &joined[cut..]);
    } else if start > 0 {
        joined = format!("... (truncated)\n{joined}");
    }
    joined
}

fn code_fence_for(content: &str) -> String {
    let mut max_run = 2usize;
    let mut current = 0usize;
    for ch in content.chars() {
        if ch == '`' {
            current += 1;
            max_run = max_run.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(max_run + 1)
}

pub fn format_ci_issues_md(reports: &[FailingCheckReport]) -> String {
    let mut out = String::from("# CI Status\n");
    for report in reports {
        let fence = code_fence_for(&report.output);
        out.push_str(&format!(
            "\n## {}\n\nDate: {}\nRun ID: {}\nCommit SHA: {}\n\nCommand: `{}`\n\nRelevant failing output:\n\n{}text\n{}\n{}\n",
            report.name,
            report.date,
            report.run_id,
            report.commit_sha,
            report.command.replace('`', "'"),
            fence,
            report.output,
            fence,
        ));
    }
    out
}

pub fn format_ci_passing_md(commit_sha: &str) -> String {
    format!("# CI Status\n\nAll checks passing for commit {commit_sha}.\n")
}

// ────────────────── Read-Only jj Helpers & Workspace Sync ──────────────────

fn jj_readonly(root: &Path) -> Command {
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
        .args([
            "--no-pager",
            "--color=never",
            "--ignore-working-copy",
            "--at-operation=@",
        ]);
    command
}

fn list_remote_bookmark_commits(root: &Path) -> Result<BTreeSet<String>> {
    let template = r#"if(remote && remote != "git" && present && normal_target, normal_target.commit_id() ++ "\n")"#;
    let output = jj_readonly(root)
        .args(["bookmark", "list", "--all-remotes", "-T", template])
        .output()
        .context("Cannot list remote jj bookmarks")?;
    if !output.status.success() {
        bail!("jj bookmark list failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|s| is_valid_sha(s))
        .map(str::to_owned)
        .collect())
}

pub fn parse_workspace_list(stdout: &str) -> Vec<WorkspaceInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let info: WorkspaceInfo = serde_json::from_str(trimmed).ok()?;
            (info.name != "default").then_some(info)
        })
        .collect()
}

fn list_non_default_workspaces(root: &Path) -> Result<Vec<WorkspaceInfo>> {
    let template = r#"json({"name" => name(), "commit_id" => target().commit_id()}) ++ "\n""#;
    let output = jj_readonly(root)
        .args(["workspace", "list", "-T", template])
        .output()
        .context("Cannot list jj workspaces")?;
    if !output.status.success() {
        bail!("jj workspace list failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_workspace_list(&text))
}

fn find_nearest_remote_ancestor_commit(root: &Path, commit_id: &str) -> Option<String> {
    if !is_valid_sha(commit_id) {
        return None;
    }
    let revset = format!("::({commit_id}) & remote_bookmarks()");
    let output = jj_readonly(root)
        .args([
            "log",
            "--no-graph",
            "-n",
            "1",
            "-r",
            &revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    is_valid_sha(&sha).then_some(sha)
}

/// Validate and resolve `<repo_root>/.workspaces/<name>`.
/// Refuses path traversal, symlinks, and missing directories.
pub fn resolve_workspace_dir(repo_root: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name == "default" {
        return None;
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return None,
    }
    let workspaces_root = repo_root.join(".workspaces");
    let root_meta = fs::symlink_metadata(&workspaces_root).ok()?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return None;
    }
    let ws_dir = workspaces_root.join(name);
    let ws_meta = fs::symlink_metadata(&ws_dir).ok()?;
    if ws_meta.file_type().is_symlink() || !ws_meta.is_dir() {
        return None;
    }
    let canon_root = fs::canonicalize(&workspaces_root).ok()?;
    let canon_ws = fs::canonicalize(&ws_dir).ok()?;
    if !canon_ws.starts_with(&canon_root) || canon_ws == canon_root {
        return None;
    }
    Some(canon_ws)
}

/// Atomically write `content` to `<ws_dir>/scratch/CI_ISSUES.md` if it differs
/// from the existing content. Refuses symlinked `scratch/` or `CI_ISSUES.md`.
/// When `only_if_exists` is true, skips creating `scratch/` or `CI_ISSUES.md`
/// if `CI_ISSUES.md` does not already exist.
pub fn write_ci_issues_atomic(ws_dir: &Path, content: &str, only_if_exists: bool) -> Result<bool> {
    let scratch_dir = ws_dir.join("scratch");
    match fs::symlink_metadata(&scratch_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!("Refusing non-directory or symlinked scratch directory");
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if only_if_exists {
                return Ok(false);
            }
            fs::create_dir(&scratch_dir).context("Cannot create workspace scratch directory")?;
        }
        Err(err) => return Err(err).context("Cannot inspect workspace scratch directory"),
    }

    let target = scratch_dir.join("CI_ISSUES.md");
    match fs::symlink_metadata(&target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                bail!("Refusing non-file or symlinked CI_ISSUES.md");
            }
            if let Ok(existing) = fs::read_to_string(&target)
                && existing == content
            {
                return Ok(false);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if only_if_exists {
                return Ok(false);
            }
        }
        Err(err) => return Err(err).context("Cannot inspect CI_ISSUES.md"),
    }

    let mut temp =
        NamedTempFile::new_in(&scratch_dir).context("Cannot create temporary CI_ISSUES.md")?;
    temp.write_all(content.as_bytes())
        .context("Cannot write temporary CI_ISSUES.md")?;
    temp.flush()
        .context("Cannot flush temporary CI_ISSUES.md")?;
    temp.persist(&target)
        .map_err(|e| e.error)
        .context("Cannot persist CI_ISSUES.md")?;
    Ok(true)
}

fn sync_all_workspaces(
    root: &Path,
    client: &Client,
    remote_shas: &BTreeSet<String>,
    state: &mut WorkerState,
) {
    let Ok(workspaces) = list_non_default_workspaces(root) else {
        return;
    };
    if workspaces.is_empty() {
        return;
    }

    let mut fetched_logs: BTreeMap<u64, FilteredLog> = BTreeMap::new();

    for ws in workspaces {
        let Some(ws_dir) = resolve_workspace_dir(root, &ws.name) else {
            continue;
        };
        let target_sha = if remote_shas.contains(&ws.commit_id) {
            Some(ws.commit_id.clone())
        } else {
            find_nearest_remote_ancestor_commit(root, &ws.commit_id)
        };
        let Some(commit_sha) = target_sha else {
            continue;
        };
        let Some(ci_state) = state
            .badge_cache
            .get(&(client.repo.clone(), commit_sha.clone()))
            .cloned()
        else {
            continue;
        };

        let failing_checks: Vec<&CheckInfo> = ci_state
            .checks
            .iter()
            .filter(|c| c.outcome == CheckOutcome::Failing)
            .collect();

        if !failing_checks.is_empty() {
            let fingerprint = SyncFingerprint::Failing {
                commit_sha: commit_sha.clone(),
                checks: failing_checks
                    .iter()
                    .map(|c| (c.name.clone(), c.job_id, c.timestamp.clone()))
                    .collect(),
            };
            if state.synced_workspaces.get(&ws.name) == Some(&fingerprint) {
                continue;
            }

            let mut reports = Vec::with_capacity(failing_checks.len());
            for check in failing_checks {
                let filtered = check.job_id.and_then(|job_id| {
                    if let Some(cached) = fetched_logs.get(&job_id) {
                        return Some(cached.clone());
                    }
                    let endpoint = format!("repos/{}/actions/jobs/{job_id}/logs", client.repo);
                    let raw =
                        github::with_background_deadlines(|| client.get_text(&endpoint)).ok()?;
                    let parsed = filter_job_logs(&raw);
                    fetched_logs.insert(job_id, parsed.clone());
                    Some(parsed)
                });

                let (command, output) = match filtered {
                    Some(log) => (
                        log.command
                            .or_else(|| check.details_url.clone())
                            .unwrap_or_else(|| check.name.clone()),
                        if log.output.is_empty() {
                            String::from("(no failure output captured)")
                        } else {
                            log.output
                        },
                    ),
                    None => (
                        check
                            .details_url
                            .clone()
                            .unwrap_or_else(|| check.name.clone()),
                        String::from("(job logs unavailable)"),
                    ),
                };

                reports.push(FailingCheckReport {
                    name: check.name.clone(),
                    date: check
                        .timestamp
                        .clone()
                        .unwrap_or_else(|| String::from("unknown")),
                    run_id: check
                        .job_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| String::from("unknown")),
                    commit_sha: commit_sha.clone(),
                    command,
                    output,
                });
            }

            let markdown = format_ci_issues_md(&reports);
            if write_ci_issues_atomic(&ws_dir, &markdown, false).is_ok() {
                state.synced_workspaces.insert(ws.name, fingerprint);
            }
        } else if ci_state.badge == Some(CiBadge::Passing) {
            let fingerprint = SyncFingerprint::Passing(commit_sha.clone());
            if state.synced_workspaces.get(&ws.name) == Some(&fingerprint) {
                continue;
            }
            let markdown = format_ci_passing_md(&commit_sha);
            if write_ci_issues_atomic(&ws_dir, &markdown, true).is_ok() {
                state.synced_workspaces.insert(ws.name, fingerprint);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(name: &str, outcome: CheckOutcome, is_required: Option<bool>) -> CheckInfo {
        CheckInfo {
            name: name.to_owned(),
            outcome,
            is_required,
            job_id: None,
            details_url: None,
            timestamp: None,
        }
    }

    #[test]
    fn aggregate_badge_handles_required_optional_passing_pending_and_empty() {
        assert_eq!(aggregate_badge(&[]), None);

        // All passing -> green ✓
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Passing, Some(true)),
                check("lint", CheckOutcome::Passing, Some(false)),
            ]),
            Some(CiBadge::Passing)
        );

        // Required failing -> red ✗ (even if optional also passes or fails)
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Failing, Some(true)),
                check("lint", CheckOutcome::Passing, Some(false)),
            ]),
            Some(CiBadge::RequiredFailed)
        );
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Failing, Some(true)),
                check("lint", CheckOutcome::Failing, Some(false)),
            ]),
            Some(CiBadge::RequiredFailed)
        );

        // All required passing + optional failing -> yellow !
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Passing, Some(true)),
                check("lint", CheckOutcome::Failing, Some(false)),
            ]),
            Some(CiBadge::OptionalFailed)
        );

        // Unknown requiredness failing -> falls back to red ✗
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Failing, None),
                check("lint", CheckOutcome::Passing, Some(false)),
            ]),
            Some(CiBadge::RequiredFailed)
        );

        // Pending with no failures -> nothing
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Passing, Some(true)),
                check("test", CheckOutcome::Pending, Some(true)),
            ]),
            None
        );

        // Pending required + failing optional -> yellow !
        assert_eq!(
            aggregate_badge(&[
                check("build", CheckOutcome::Pending, Some(true)),
                check("lint", CheckOutcome::Failing, Some(false)),
            ]),
            Some(CiBadge::OptionalFailed)
        );
    }

    #[test]
    fn fault_isolation_on_malformed_or_error_graphql_responses() {
        let sha = "a".repeat(40);
        // API error / missing data yields Err (silently ignored by worker -> no badge)
        let err_resp = json!({ "errors": [{ "message": "API rate limit exceeded" }] });
        assert!(parse_rollup_response(&err_resp, std::slice::from_ref(&sha)).is_err());

        // Null commit object (unpushed / unknown SHA) yields empty list -> no badge
        let null_resp = json!({ "data": { "repository": { "c0": null } } });
        let parsed = parse_rollup_response(&null_resp, std::slice::from_ref(&sha)).unwrap();
        assert!(parsed.is_empty());

        // Empty rollup yields None badge
        let empty_resp = json!({
            "data": {
                "repository": {
                    "c0": {
                        "oid": sha,
                        "associatedPullRequests": { "nodes": [] },
                        "statusCheckRollup": null
                    }
                }
            }
        });
        let parsed = parse_rollup_response(&empty_resp, std::slice::from_ref(&sha)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].badge, None);
    }

    #[test]
    fn rollup_parsing_and_required_overlay() {
        let sha = "b".repeat(40);
        let resp = json!({
            "data": {
                "repository": {
                    "c0": {
                        "oid": sha,
                        "associatedPullRequests": {
                            "nodes": [{ "number": 42, "state": "OPEN", "headRefOid": sha }]
                        },
                        "statusCheckRollup": {
                            "contexts": {
                                "nodes": [
                                    {
                                        "__typename": "CheckRun",
                                        "name": "required-test",
                                        "status": "COMPLETED",
                                        "conclusion": "SUCCESS",
                                        "detailsUrl": "https://github.com/o/r/actions/runs/1/job/111",
                                        "completedAt": "2026-11-25T10:00:00Z",
                                        "databaseId": 111
                                    },
                                    {
                                        "__typename": "CheckRun",
                                        "name": "optional-lint",
                                        "status": "COMPLETED",
                                        "conclusion": "FAILURE",
                                        "detailsUrl": "https://github.com/o/r/actions/runs/1/job/222",
                                        "completedAt": "2026-11-25T10:01:00Z",
                                        "databaseId": 222
                                    }
                                ]
                            }
                        }
                    }
                }
            }
        });
        let mut states = parse_rollup_response(&resp, std::slice::from_ref(&sha)).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].pr_number, Some(42));
        // Before requiredness overlay, unknown requiredness falls back to RequiredFailed.
        assert_eq!(states[0].badge, Some(CiBadge::RequiredFailed));

        let req_resp = json!({
            "data": {
                "repository": {
                    "r0": {
                        "statusCheckRollup": {
                            "contexts": {
                                "nodes": [
                                    { "__typename": "CheckRun", "name": "required-test", "isRequired": true },
                                    { "__typename": "CheckRun", "name": "optional-lint", "isRequired": false }
                                ]
                            }
                        }
                    }
                }
            }
        });
        apply_required_response(&req_resp, &[(0, sha.clone(), 42)], &mut states);
        assert_eq!(states[0].badge, Some(CiBadge::OptionalFailed));
    }

    #[test]
    fn job_id_extraction_and_log_filtering() {
        assert_eq!(
            extract_job_id_from_url("https://github.com/org/repo/actions/runs/123/job/987654321"),
            Some(987654321)
        );
        assert_eq!(
            extract_job_id_from_url("https://github.com/org/repo/actions/runs/123"),
            None
        );

        let raw = "\
2026-11-25T12:00:00.1234567Z ##[group]Run cargo test --workspace\r\n\
2026-11-25T12:00:00.1234568Z cargo test --workspace\r\n\
2026-11-25T12:00:00.1234569Z ##[endgroup]\r\n\
2026-11-25T12:00:01.0000000Z running 3 tests\r\n\
2026-11-25T12:00:01.2000000Z test alpha ... FAILED\r\n\
2026-11-25T12:00:01.3000000Z ##[error]Process completed with exit code 101.\r\n\
2026-11-25T12:00:02.0000000Z ##[group]Post job cleanup\r\n\
2026-11-25T12:00:02.1000000Z Cleaning up\r\n\
2026-11-25T12:00:02.2000000Z ##[endgroup]\r\n";

        let filtered = filter_job_logs(raw);
        assert_eq!(filtered.command.as_deref(), Some("cargo test --workspace"));
        assert!(filtered.output.contains("test alpha ... FAILED"));
        assert!(!filtered.output.contains("2026-11-25T"));
        assert!(!filtered.output.contains("Cleaning up"));
    }

    #[test]
    fn markdown_formatting_and_idempotent_workspace_sync() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join(".workspaces/agent1");
        fs::create_dir_all(&ws).unwrap();

        let report = FailingCheckReport {
            name: "unit-tests".into(),
            date: "2026-11-25T12:00:01Z".into(),
            run_id: "987654321".into(),
            commit_sha: "c".repeat(40),
            command: "cargo test".into(),
            output: "test alpha ... FAILED".into(),
        };
        let md = format_ci_issues_md(&[report]);
        assert!(md.contains("# CI Status"));
        assert!(md.contains("## unit-tests"));
        assert!(md.contains("Date: 2026-11-25T12:00:01Z"));
        assert!(md.contains("Run ID: 987654321"));
        assert!(md.contains("Command: `cargo test`"));
        assert!(md.contains("```text\ntest alpha ... FAILED\n```"));

        // First write creates scratch/CI_ISSUES.md.
        assert!(write_ci_issues_atomic(&ws, &md, false).unwrap());
        // Identical content is a no-op.
        assert!(!write_ci_issues_atomic(&ws, &md, false).unwrap());

        // Recovery to passing updates the file.
        let passing = format_ci_passing_md(&"d".repeat(40));
        assert!(write_ci_issues_atomic(&ws, &passing, true).unwrap());
        let on_disk = fs::read_to_string(ws.join("scratch/CI_ISSUES.md")).unwrap();
        assert_eq!(on_disk, passing);

        // Passing state on a clean workspace with no prior CI_ISSUES.md does nothing.
        let ws2 = dir.path().join(".workspaces/agent2");
        fs::create_dir_all(&ws2).unwrap();
        assert!(!write_ci_issues_atomic(&ws2, &passing, true).unwrap());
        assert!(!ws2.join("scratch/CI_ISSUES.md").exists());
    }

    #[test]
    fn non_default_workspace_detection_and_path_safety() {
        let listing = "\
{\"name\":\"default\",\"commit_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n\
{\"name\":\"fix-ci\",\"commit_id\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"}\n";
        let workspaces = parse_workspace_list(listing);
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "fix-ci");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".workspaces/fix-ci")).unwrap();

        // Valid workspace resolves inside .workspaces/.
        assert!(resolve_workspace_dir(root, "fix-ci").is_some());
        // Rejects default, missing, empty, and traversal names.
        assert!(resolve_workspace_dir(root, "default").is_none());
        assert!(resolve_workspace_dir(root, "missing").is_none());
        assert!(resolve_workspace_dir(root, "").is_none());
        assert!(resolve_workspace_dir(root, "../escape").is_none());
        assert!(resolve_workspace_dir(root, "a/b").is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), root.join(".workspaces/linked")).unwrap();
            assert!(resolve_workspace_dir(root, "linked").is_none());

            // Also reject symlinked CI_ISSUES.md inside a valid workspace.
            let scratch = root.join(".workspaces/fix-ci/scratch");
            fs::create_dir_all(&scratch).unwrap();
            let target = outside.path().join("victim.md");
            fs::write(&target, "keep").unwrap();
            symlink(&target, scratch.join("CI_ISSUES.md")).unwrap();
            assert!(
                write_ci_issues_atomic(&root.join(".workspaces/fix-ci"), "overwrite", false)
                    .is_err()
            );
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        }
    }
}
