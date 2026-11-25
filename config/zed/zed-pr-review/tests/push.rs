//! End-to-end integration tests for `zed-pr-review push` with real `jj`, a real
//! bare Git `origin` remote, and a stateful fake `gh` CLI.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "owner/project";
const BIN: &str = env!("CARGO_BIN_EXE_zed-pr-review");

fn write_executable(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn run_ok(cmd: &mut Command) -> Result<Output> {
    let output = cmd.output().context("spawn command")?;
    if !output.status.success() {
        anyhow::bail!(
            "command failed: {:?}\nstdout:\n{}\nstderr:\n{}",
            cmd,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

fn jj_ok(root: &Path, args: &[&str]) -> Result<String> {
    let output = run_ok(
        Command::new("jj")
            .current_dir(root)
            .env("JJ_PAGER", "cat")
            .args(["--no-pager", "--color=never"])
            .args(args),
    )?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

struct PushFixture {
    _temp: TempDir,
    root: PathBuf,
    server: PathBuf,
    bin_dir: PathBuf,
    gh_state: PathBuf,
    gh_calls: PathBuf,
}

impl PushFixture {
    fn new() -> Result<Self> {
        let temp = TempDir::new()?;
        let root = temp.path().join("repo");
        let server = temp.path().join("origin.git");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&bin_dir)?;

        run_ok(
            Command::new("git")
                .args(["init", "--bare", "--initial-branch=main"])
                .arg(&server),
        )?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["git", "init", "--colocate"]),
        )?;
        jj_ok(&root, &["config", "set", "--repo", "user.name", "Stefan"])?;
        jj_ok(
            &root,
            &[
                "config",
                "set",
                "--repo",
                "user.email",
                "stefan@example.com",
            ],
        )?;
        jj_ok(
            &root,
            &["git", "remote", "add", "origin", server.to_str().unwrap()],
        )?;

        // Initial commit on main, pushed to origin.
        fs::create_dir_all(root.join(".github"))?;
        fs::write(
            root.join(".github/pull_request_template.md"),
            "## Summary\n\n<!-- template -->\n",
        )?;
        fs::write(root.join("base.txt"), "base\n")?;
        jj_ok(&root, &["describe", "-m", "initial commit"])?;
        jj_ok(&root, &["bookmark", "create", "main", "-r", "@"])?;
        jj_ok(
            &root,
            &["git", "push", "--remote", "origin", "--bookmark", "main"],
        )?;
        // Set revset alias `trunk()` -> `main@origin` and `immutable_bookmarks()` -> `main`.
        jj_ok(
            &root,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.\"trunk()\"",
                "main@origin",
            ],
        )?;
        jj_ok(
            &root,
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.\"immutable_bookmarks()\"",
                "main",
            ],
        )?;

        let gh_state = temp.path().join("gh-state.json");
        let gh_calls = temp.path().join("gh-calls.jsonl");
        fs::write(
            &gh_state,
            serde_json::to_vec_pretty(&json!({
                "user": "stefan",
                "next_number": 100,
                "pulls": []
            }))?,
        )?;
        fs::write(&gh_calls, "")?;

        let fake_gh = format!(
            r#"#!/usr/bin/env python3
import json, os, sys, urllib.parse

STATE = {state:?}
CALLS = {calls:?}
REPO = {repo:?}

def load_state():
    with open(STATE, "r") as f:
        return json.load(f)

def save_state(s):
    with open(STATE, "w") as f:
        json.dump(s, f, indent=2)

def log_call(entry):
    with open(CALLS, "a") as f:
        f.write(json.dumps(entry) + "\n")

args = sys.argv[1:]

if args[:2] == ["repo", "view"]:
    print(json.dumps({{"nameWithOwner": REPO, "url": f"https://github.com/{{REPO}}"}}))
    sys.exit(0)

if args and args[0] == "api":
    method = "GET"
    endpoint = None
    paginate = False
    read_stdin = False
    i = 1
    while i < len(args):
        a = args[i]
        if a == "--method":
            method = args[i + 1]
            i += 2
        elif a in ("--hostname", "--header"):
            i += 2
        elif a == "--input":
            read_stdin = (args[i + 1] == "-")
            i += 2
        elif a in ("--paginate", "--slurp"):
            paginate = True
            i += 1
        elif a.startswith("-"):
            i += 1
        else:
            endpoint = a
            i += 1

    body = None
    if read_stdin:
        raw = sys.stdin.read()
        if raw.strip():
            body = json.loads(raw)

    log_call({{"method": method, "endpoint": endpoint, "body": body}})
    state = load_state()

    if endpoint == "user" and method == "GET":
        print(json.dumps({{"login": state["user"]}}))
        sys.exit(0)

    if endpoint == "graphql" and method == "POST":
        node_id = body["variables"]["id"]
        for p in state["pulls"]:
            if p.get("node_id") == node_id:
                p["draft"] = False
        save_state(state)
        print(json.dumps({{
            "data": {{
                "markPullRequestReadyForReview": {{
                    "pullRequest": {{"id": node_id, "isDraft": False}}
                }}
            }}
        }}))
        sys.exit(0)

    parsed = urllib.parse.urlsplit(endpoint)
    path = parsed.path
    qs = urllib.parse.parse_qs(parsed.query)

    if path == f"repos/{{REPO}}/pulls" and method == "GET":
        head_filter = qs.get("head", [None])[0]
        matches = []
        for p in state["pulls"]:
            if head_filter:
                _, branch = head_filter.split(":", 1)
                if p["head"]["ref"] != branch:
                    continue
            matches.append(p)
        if paginate:
            print(json.dumps([matches]))
        else:
            print(json.dumps(matches))
        sys.exit(0)

    if path == f"repos/{{REPO}}/pulls" and method == "POST":
        num = state["next_number"]
        state["next_number"] += 1
        pr = {{
            "number": num,
            "node_id": f"PR_node_{{num}}",
            "state": "open",
            "merged_at": None,
            "draft": bool(body.get("draft", False)),
            "title": body["title"],
            "body": body.get("body", ""),
            "html_url": f"https://github.com/{{REPO}}/pull/{{num}}",
            "user": {{"login": state["user"]}},
            "head": {{"ref": body["head"], "sha": "a" * 40}},
            "base": {{"ref": body["base"], "sha": "b" * 40}},
        }}
        state["pulls"].append(pr)
        save_state(state)
        print(json.dumps(pr))
        sys.exit(0)

    if path.startswith(f"repos/{{REPO}}/pulls/") and method == "PATCH":
        num = int(path.rsplit("/", 1)[1])
        for p in state["pulls"]:
            if p["number"] == num:
                if "base" in body:
                    p["base"]["ref"] = body["base"]
                if "title" in body:
                    p["title"] = body["title"]
                if "state" in body:
                    p["state"] = body["state"]
                save_state(state)
                print(json.dumps(p))
                sys.exit(0)
        sys.stderr.write(f"PR #{{num}} not found\n")
        sys.exit(1)

sys.stderr.write(f"Unhandled gh invocation: {{args}}\n")
sys.exit(1)
"#,
            state = gh_state.to_str().unwrap(),
            calls = gh_calls.to_str().unwrap(),
            repo = REPO,
        );
        write_executable(&bin_dir.join("gh"), &fake_gh)?;

        Ok(Self {
            _temp: temp,
            root,
            server,
            bin_dir,
            gh_state,
            gh_calls,
        })
    }

    fn run_push(&self, args: &[&str]) -> Result<Output> {
        let mut paths = vec![self.bin_dir.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").context("PATH")?,
        ));
        let output = Command::new(BIN)
            .args(["--directory"])
            .arg(&self.root)
            .arg("push")
            .args(args)
            .env("PATH", std::env::join_paths(paths)?)
            .env("JJ_PAGER", "cat")
            .output()
            .context("run zed-pr-review push")?;
        Ok(output)
    }

    fn read_calls(&self) -> Result<Vec<Value>> {
        let text = fs::read_to_string(&self.gh_calls)?;
        let mut calls = Vec::new();
        for line in text.lines() {
            if !line.trim().is_empty() {
                calls.push(serde_json::from_str(line)?);
            }
        }
        Ok(calls)
    }

    fn clear_calls(&self) -> Result<()> {
        fs::write(&self.gh_calls, "")?;
        Ok(())
    }

    fn read_state(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&fs::read(&self.gh_state)?)?)
    }

    fn write_state(&self, value: &Value) -> Result<()> {
        fs::write(&self.gh_state, serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }

    fn remote_branches(&self) -> Result<Vec<String>> {
        let out = run_ok(
            Command::new("git")
                .arg("--git-dir")
                .arg(&self.server)
                .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"]),
        )?;
        let mut branches: Vec<String> = String::from_utf8(out.stdout)?
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        branches.sort();
        Ok(branches)
    }

    fn add_commit(
        &self,
        parent: &str,
        file: &str,
        content: &str,
        msg: &str,
        bm: &str,
    ) -> Result<()> {
        jj_ok(&self.root, &["new", parent, "-m", msg])?;
        fs::write(self.root.join(file), content)?;
        jj_ok(&self.root, &["bookmark", "create", bm, "-r", "@"])?;
        Ok(())
    }
}

#[test]
fn unpushed_stack_pushes_up_to_at_and_rerun_is_noop() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "a.txt", "a\n", "First feature", "stefan/feat1")?;
    fx.add_commit(
        "stefan/feat1",
        "b.txt",
        "b\n",
        "Second feature",
        "stefan/feat2",
    )?;
    fx.add_commit("stefan/feat2", "c.txt", "c\n", "Third WIP", "stefan/feat3")?;
    // Move @ back to feat2 so feat3 is an unpushed descendant above @.
    jj_ok(&fx.root, &["edit", "stefan/feat2"])?;

    let out = fx.run_push(&["--json"])?;
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(report["host"], "github.com");
    assert_eq!(report["repo"], REPO);
    assert_eq!(report["dry_run"], false);
    let stack = report["stack"].as_array().unwrap();
    assert_eq!(stack.len(), 2);
    assert_eq!(stack[0]["bookmark"], "stefan/feat1");
    assert_eq!(stack[0]["base"], "main");
    assert_eq!(stack[0]["draft"], true);
    assert_eq!(stack[0]["action"], "created");
    assert_eq!(stack[1]["bookmark"], "stefan/feat2");
    assert_eq!(stack[1]["base"], "stefan/feat1");
    assert_eq!(stack[1]["draft"], true);
    assert_eq!(stack[1]["action"], "created");

    // Verify feat3 was NOT pushed to origin.
    assert_eq!(
        fx.remote_branches()?,
        vec!["main", "stefan/feat1", "stefan/feat2"]
    );

    // Verify PR template was used as body for newly created PRs.
    let state = fx.read_state()?;
    assert_eq!(state["pulls"][0]["body"], "## Summary\n\n<!-- template -->");

    // Re-running push when nothing changed must make zero POST/PATCH calls.
    fx.clear_calls()?;
    let out2 = fx.run_push(&["--json"])?;
    assert!(out2.status.success());
    let report2: Value = serde_json::from_slice(&out2.stdout)?;
    assert_eq!(report2["stack"][0]["action"], "unchanged");
    assert_eq!(report2["stack"][1]["action"], "unchanged");

    let mutations: Vec<_> = fx
        .read_calls()?
        .into_iter()
        .filter(|c| c["method"] != "GET")
        .collect();
    assert!(
        mutations.is_empty(),
        "expected zero mutations on no-op rerun, got: {mutations:?}"
    );
    Ok(())
}

#[test]
fn pushed_stack_updates_descendants_and_retargets_without_touching_body() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "a.txt", "a\n", "First feature", "stefan/feat1")?;
    fx.add_commit(
        "stefan/feat1",
        "b.txt",
        "b\n",
        "Second feature",
        "stefan/feat2",
    )?;
    fx.add_commit("stefan/feat2", "c.txt", "c\n", "Third WIP", "stefan/feat3")?;
    jj_ok(&fx.root, &["edit", "stefan/feat2"])?;

    // Push feat1 and feat2 initially.
    let init = fx.run_push(&["--json"])?;
    assert!(init.status.success());

    // Simulate author prose + branch-stack metadata in feat2's PR body.
    let mut state = fx.read_state()?;
    state["pulls"][1]["body"] = json!("Custom author prose\n<!-- branch-stack -->");
    fx.write_state(&state)?;

    // Insert a new revision feat1b between feat1 and feat2, with @ on feat1b.
    jj_ok(&fx.root, &["new", "stefan/feat1", "-m", "Inserted middle"])?;
    fs::write(fx.root.join("mid.txt"), "mid\n")?;
    jj_ok(
        &fx.root,
        &["bookmark", "create", "stefan/feat1b", "-r", "@"],
    )?;
    jj_ok(
        &fx.root,
        &["rebase", "-s", "stefan/feat2", "-d", "stefan/feat1b"],
    )?;

    fx.clear_calls()?;
    let out = fx.run_push(&["--json"])?;
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout)?;
    let stack = report["stack"].as_array().unwrap();
    // Should include feat1, feat1b, and already-pushed descendant feat2 — NOT unpushed feat3.
    assert_eq!(stack.len(), 3);
    assert_eq!(stack[0]["bookmark"], "stefan/feat1");
    assert_eq!(stack[0]["action"], "unchanged");
    assert_eq!(stack[1]["bookmark"], "stefan/feat1b");
    assert_eq!(stack[1]["base"], "stefan/feat1");
    assert_eq!(stack[1]["action"], "created");
    assert_eq!(stack[2]["bookmark"], "stefan/feat2");
    assert_eq!(stack[2]["base"], "stefan/feat1b");
    assert_eq!(stack[2]["action"], "retargeted");

    // Verify PATCH never sent a `body` field and author body is preserved.
    let patches: Vec<_> = fx
        .read_calls()?
        .into_iter()
        .filter(|c| c["method"] == "PATCH")
        .collect();
    assert_eq!(patches.len(), 1);
    assert!(patches[0]["body"].get("body").is_none());
    assert_eq!(patches[0]["body"]["base"], "stefan/feat1b");

    let state_after = fx.read_state()?;
    assert_eq!(
        state_after["pulls"][1]["body"],
        "Custom author prose\n<!-- branch-stack -->"
    );
    Ok(())
}

#[test]
fn main_moved_ahead_with_stale_bookmark_still_targets_main() -> Result<()> {
    let fx = PushFixture::new()?;
    // Create an old stale bookmark on the initial main commit (simulating a merged/closed PR).
    jj_ok(
        &fx.root,
        &["bookmark", "create", "old/closed-pr", "-r", "main"],
    )?;
    // Create our stack based on the initial main commit.
    fx.add_commit("main", "a.txt", "a\n", "My feature", "stefan/feat1")?;

    // Now advance main ahead on origin AND locally without rebasing stefan/feat1.
    jj_ok(
        &fx.root,
        &["new", "main", "-m", "other merged commit on main"],
    )?;
    fs::write(fx.root.join("other.txt"), "other\n")?;
    jj_ok(&fx.root, &["bookmark", "set", "main", "-r", "@"])?;
    jj_ok(
        &fx.root,
        &["git", "push", "--remote", "origin", "--bookmark", "main"],
    )?;

    // Also test local main moved even further ahead without pushing.
    jj_ok(&fx.root, &["new", "main", "-m", "local main ahead"])?;
    fs::write(fx.root.join("local_main.txt"), "local\n")?;
    jj_ok(&fx.root, &["bookmark", "set", "main", "-r", "@"])?;

    // Switch back to our un-rebased stack.
    jj_ok(&fx.root, &["edit", "stefan/feat1"])?;

    let out = fx.run_push(&["--json"])?;
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(report["stack"].as_array().unwrap().len(), 1);
    assert_eq!(report["stack"][0]["bookmark"], "stefan/feat1");
    assert_eq!(report["stack"][0]["base"], "main");
    Ok(())
}

#[test]
fn dry_run_does_not_push_or_mutate_github() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "a.txt", "a\n", "First feature", "stefan/feat1")?;

    let out = fx.run_push(&["--dry-run", "--json"])?;
    assert!(out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["stack"][0]["bookmark"], "stefan/feat1");
    assert_eq!(report["stack"][0]["base"], "main");
    assert_eq!(report["stack"][0]["action"], "created");
    assert!(report["stack"][0]["pr_number"].is_null());

    // Origin must still only have `main`.
    assert_eq!(fx.remote_branches()?, vec!["main"]);
    let mutations: Vec<_> = fx
        .read_calls()?
        .into_iter()
        .filter(|c| c["method"] != "GET")
        .collect();
    assert!(mutations.is_empty());
    Ok(())
}

#[test]
fn refuses_review_stack_and_foreign_pr_owner() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "r.txt", "r\n", "Reviewing PR", "review/pr-42")?;
    let out = fx.run_push(&[])?;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("review/pr-*"), "stderr: {stderr}");

    // Switch to a normal branch that collides with another user's PR on GitHub.
    fx.add_commit("main", "a.txt", "a\n", "My change", "shared/branch")?;
    fx.write_state(&json!({
        "user": "stefan",
        "next_number": 200,
        "pulls": [{
            "number": 77,
            "node_id": "PR_77",
            "state": "open",
            "merged_at": null,
            "draft": false,
            "title": "Alice's PR",
            "html_url": "https://github.com/owner/project/pull/77",
            "user": {"login": "alice"},
            "head": {"ref": "shared/branch", "sha": "a".repeat(40)},
            "base": {"ref": "main", "sha": "b".repeat(40)}
        }]
    }))?;

    let out2 = fx.run_push(&[])?;
    assert!(!out2.status.success());
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(stderr2.contains("owned by @alice"), "stderr: {stderr2}");
    // Must not have pushed the branch before checking ownership.
    assert_eq!(fx.remote_branches()?, vec!["main"]);
    Ok(())
}

#[test]
fn reopens_closed_pr_and_supports_update_title_and_ready() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "a.txt", "a\n", "Updated title", "stefan/feat1")?;
    fx.write_state(&json!({
        "user": "stefan",
        "next_number": 200,
        "pulls": [{
            "number": 55,
            "node_id": "PR_55",
            "state": "closed",
            "merged_at": null,
            "draft": true,
            "title": "Old title",
            "body": "Keep this body",
            "html_url": "https://github.com/owner/project/pull/55",
            "user": {"login": "stefan"},
            "head": {"ref": "stefan/feat1", "sha": "a".repeat(40)},
            "base": {"ref": "old-base", "sha": "b".repeat(40)}
        }]
    }))?;

    let out = fx.run_push(&["--update-title", "--ready", "--json"])?;
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(report["stack"][0]["action"], "reopened");
    assert_eq!(report["stack"][0]["base"], "main");
    assert_eq!(report["stack"][0]["title"], "Updated title");
    assert_eq!(report["stack"][0]["draft"], false);

    let state = fx.read_state()?;
    assert_eq!(state["pulls"][0]["state"], "open");
    assert_eq!(state["pulls"][0]["body"], "Keep this body");
    Ok(())
}

#[test]
fn refuses_empty_description_unless_allowed() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "a.txt", "a\n", "", "stefan/empty")?;

    let err_out = fx.run_push(&[])?;
    assert!(!err_out.status.success());
    assert!(String::from_utf8_lossy(&err_out.stderr).contains("empty description"));

    let ok_out = fx.run_push(&["--allow-empty-description", "--json"])?;
    assert!(ok_out.status.success());
    let report: Value = serde_json::from_slice(&ok_out.stdout)?;
    assert_eq!(report["stack"][0]["title"], "stefan/empty");
    Ok(())
}

#[test]
fn revset_override_and_force_new_pr() -> Result<()> {
    let fx = PushFixture::new()?;
    fx.add_commit("main", "a.txt", "a\n", "First feature", "stefan/feat1")?;
    fx.add_commit(
        "stefan/feat1",
        "b.txt",
        "b\n",
        "Second feature",
        "stefan/feat2",
    )?;
    // Even though @ is on feat2, -r stefan/feat1 should push only feat1.
    let out = fx.run_push(&["-r", "stefan/feat1", "--json"])?;
    assert!(out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(report["stack"].as_array().unwrap().len(), 1);
    assert_eq!(report["stack"][0]["bookmark"], "stefan/feat1");
    assert_eq!(report["stack"][0]["pr_number"], 100);

    // --force-new-pr creates a new PR instead of reusing #100.
    let out2 = fx.run_push(&["-r", "stefan/feat1", "--force-new-pr", "--json"])?;
    assert!(out2.status.success());
    let report2: Value = serde_json::from_slice(&out2.stdout)?;
    assert_eq!(report2["stack"][0]["action"], "created");
    assert_eq!(report2["stack"][0]["pr_number"], 101);
    Ok(())
}

#[test]
fn refuses_conflicts_and_ambiguous_multi_bookmark() -> Result<()> {
    let fx = PushFixture::new()?;
    // Create a conflict by editing base.txt differently on two branches and rebasing.
    fx.add_commit("main", "base.txt", "branch a\n", "Branch A", "stefan/a")?;
    fx.add_commit("main", "base.txt", "branch b\n", "Branch B", "stefan/b")?;
    jj_ok(&fx.root, &["rebase", "-r", "stefan/b", "-d", "stefan/a"])?;
    jj_ok(&fx.root, &["edit", "stefan/b"])?;

    let conflict_out = fx.run_push(&[])?;
    assert!(!conflict_out.status.success());
    assert!(
        String::from_utf8_lossy(&conflict_out.stderr).contains("conflicted"),
        "stderr: {}",
        String::from_utf8_lossy(&conflict_out.stderr)
    );

    // Switch to a clean commit with two local bookmarks.
    fx.add_commit("main", "clean.txt", "clean\n", "Clean commit", "stefan/one")?;
    jj_ok(&fx.root, &["bookmark", "create", "stefan/two", "-r", "@"])?;
    let multi_out = fx.run_push(&[])?;
    assert!(!multi_out.status.success());
    assert!(
        String::from_utf8_lossy(&multi_out.stderr).contains("multiple local bookmarks"),
        "stderr: {}",
        String::from_utf8_lossy(&multi_out.stderr)
    );
    Ok(())
}
