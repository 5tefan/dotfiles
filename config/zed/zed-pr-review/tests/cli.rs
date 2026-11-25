//! End-to-end integration tests for the `jj` review-stack workflow.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result};
use serde_json::json;
use tempfile::TempDir;

const REPO: &str = "owner/project";
const BIN: &str = env!("CARGO_BIN_EXE_zed-pr-review");

fn real_git() -> PathBuf {
    for path in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
        if Path::new(path).exists() {
            return PathBuf::from(path);
        }
    }
    PathBuf::from("git")
}

fn write_executable(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

struct StackFixture {
    _temp: TempDir,
    root: PathBuf,
    bin_dir: PathBuf,
    gh_calls: PathBuf,
    editor: PathBuf,
    previous_commit: String,
}

impl StackFixture {
    fn new() -> Result<Self> {
        let temp = TempDir::new()?;
        let root = temp.path().join("repo");
        let server = temp.path().join("server.git");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&bin_dir)?;

        let git_bin = real_git();
        run_ok(Command::new(&git_bin).args(["init", "--bare"]).arg(&server))?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["git", "init", "--colocate"]),
        )?;
        run_ok(Command::new("jj").current_dir(&root).args([
            "config",
            "set",
            "--repo",
            "user.name",
            "Stefan",
        ]))?;
        run_ok(Command::new("jj").current_dir(&root).args([
            "config",
            "set",
            "--repo",
            "user.email",
            "stefan@example.com",
        ]))?;
        run_ok(Command::new("jj").current_dir(&root).args([
            "config",
            "set",
            "--repo",
            "aliases.sb",
            r#"["edit", "@-"]"#,
        ]))?;
        run_ok(Command::new("jj").current_dir(&root).args([
            "config",
            "set",
            "--repo",
            "aliases.sf",
            r#"["edit", "@+"]"#,
        ]))?;
        run_ok(Command::new("jj").current_dir(&root).args([
            "config",
            "set",
            "--repo",
            "aliases.l",
            r#"["log", "-r", "main | immutable_heads()-..@ | @::"]"#,
        ]))?;

        // Base commit on main.
        fs::write(root.join("base.txt"), "base line\n")?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["commit", "-m", "base"]),
        )?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["bookmark", "create", "main", "-r", "@-"]),
        )?;
        let main_sha = jj_output(
            &root,
            &["log", "-r", "main", "--no-graph", "-T", "commit_id"],
        )?;

        // PR #101: two commits (squashed into 1 review revision by `open`).
        fs::write(root.join("one.txt"), "alpha\nbeta\n")?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["commit", "-m", "pr 101 commit 1"]),
        )?;
        fs::write(root.join("two.txt"), "gamma\n")?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["commit", "-m", "pr 101 commit 2"]),
        )?;
        let pr1_head = jj_output(&root, &["log", "-r", "@-", "--no-graph", "-T", "commit_id"])?;

        // PR #102: stacked on PR #101.
        fs::write(root.join("three.txt"), "delta\nepsilon\n")?;
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["commit", "-m", "pr 102 commit"]),
        )?;
        let pr2_head = jj_output(&root, &["log", "-r", "@-", "--no-graph", "-T", "commit_id"])?;

        // Push refs to local bare server.
        run_ok(Command::new(&git_bin).current_dir(&root).args([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/project.git",
        ]))?;
        for spec in [
            format!("{main_sha}:refs/heads/main"),
            format!("{pr1_head}:refs/pull/101/head"),
            format!("{pr1_head}:refs/heads/feat/pr-101"),
            format!("{pr2_head}:refs/pull/102/head"),
            format!("{pr2_head}:refs/heads/feat/pr-102"),
        ] {
            run_ok(Command::new(&git_bin).current_dir(&root).args([
                "push",
                server.to_str().unwrap(),
                &spec,
            ]))?;
        }

        // Return @ to user's own WIP on main.
        run_ok(
            Command::new("jj")
                .current_dir(&root)
                .args(["new", "main", "-m", "my wip"]),
        )?;
        let previous_commit =
            jj_output(&root, &["log", "-r", "@", "--no-graph", "-T", "commit_id"])?;

        // Fake `git` wrapper that redirects https://github.com/owner/project.git -> local bare server on fetch.
        let git_wrapper = format!(
            r#"#!/usr/bin/env python3
import os, sys
real_git = {real_git:?}
server = {server:?}
args = sys.argv[1:]
if "fetch" in args:
    os.environ["GIT_ALLOW_PROTOCOL"] = "file:https"
    idx = args.index("fetch")
    args = args[:idx] + ["-c", "protocol.file.allow=always"] + args[idx:]
    args = [server if a == "https://github.com/owner/project.git" else a for a in args]
os.execv(real_git, [real_git] + args)
"#,
            real_git = git_bin.to_str().unwrap(),
            server = server.to_str().unwrap(),
        );
        write_executable(&bin_dir.join("git"), &git_wrapper)?;

        // Fake `gh` serving PR #101 and PR #102 and recording reviews.
        let pr1_json = json!({
            "number": 101,
            "state": "open",
            "merged": false,
            "merged_at": null,
            "draft": false,
            "html_url": "https://github.com/owner/project/pull/101",
            "title": "First PR",
            "user": { "login": "alice" },
            "head": { "sha": pr1_head, "ref": "feat/pr-101", "repo": { "full_name": REPO } },
            "base": { "sha": main_sha, "ref": "main", "repo": { "full_name": REPO } }
        });
        let pr2_json = json!({
            "number": 102,
            "state": "open",
            "merged": false,
            "merged_at": null,
            "draft": false,
            "html_url": "https://github.com/owner/project/pull/102",
            "title": "Second PR",
            "user": { "login": "alice" },
            "head": { "sha": pr2_head, "ref": "feat/pr-102", "repo": { "full_name": REPO } },
            "base": { "sha": pr1_head, "ref": "feat/pr-101", "repo": { "full_name": REPO } }
        });

        let control = temp.path().join("gh-control.json");
        let gh_calls = temp.path().join("gh-calls.jsonl");
        fs::write(
            &control,
            serde_json::to_vec_pretty(&json!({
                "pulls": [pr1_json, pr2_json],
                "pulls_by_number": { "101": pr1_json, "102": pr2_json },
                "files_by_number": {
                    "101": [
                        { "filename": "one.txt", "status": "added", "patch": "@@ -0,0 +1,2 @@\n+alpha\n+beta" },
                        { "filename": "two.txt", "status": "added", "patch": "@@ -0,0 +1 @@\n+gamma" }
                    ],
                    "102": [
                        { "filename": "three.txt", "status": "added", "patch": "@@ -0,0 +1,2 @@\n+delta\n+epsilon" }
                    ]
                }
            }))?,
        )?;

        let gh_script = format!(
            r#"#!/usr/bin/env python3
import json, sys
control_path = {control:?}
calls_path = {calls:?}
args = sys.argv[1:]
with open(calls_path, "a") as f:
    f.write(json.dumps(args) + "\n")
with open(control_path) as f:
    ctrl = json.load(f)

if args[:2] == ["repo", "view"]:
    print(json.dumps({{"nameWithOwner": "owner/project", "url": "https://github.com/owner/project"}}))
    sys.exit(0)

if args and args[0] == "api":
    method = "GET"
    endpoint = None
    slurp = "--slurp" in args
    i = 1
    while i < len(args):
        if args[i] in ("--hostname", "--header", "--input"):
            i += 2
        elif args[i] == "--method":
            method = args[i+1]
            i += 2
        elif args[i].startswith("-"):
            i += 1
        else:
            endpoint = args[i]
            i += 1
    if endpoint == "user":
        print(json.dumps({{"login": "stefan"}}))
        sys.exit(0)
    if endpoint == "repos/owner/project/pulls?state=open&per_page=100":
        out = [ctrl["pulls"]] if slurp else ctrl["pulls"]
        print(json.dumps(out))
        sys.exit(0)
    for num in ("101", "102"):
        if endpoint == f"repos/owner/project/pulls/{{num}}":
            print(json.dumps(ctrl["pulls_by_number"][num]))
            sys.exit(0)
        if endpoint == f"repos/owner/project/pulls/{{num}}/files?per_page=100":
            files = ctrl["files_by_number"][num]
            print(json.dumps([files] if slurp else files))
            sys.exit(0)
        if endpoint == f"repos/owner/project/pulls/{{num}}/reviews?per_page=100":
            print(json.dumps([[]] if slurp else []))
            sys.exit(0)
        if endpoint == f"repos/owner/project/pulls/{{num}}/reviews" and method == "POST":
            payload = json.loads(sys.stdin.read())
            state_map = {{"APPROVE": "APPROVED", "COMMENT": "COMMENTED", "REQUEST_CHANGES": "CHANGES_REQUESTED"}}
            print(json.dumps({{
                "id": 777,
                "html_url": f"https://github.com/owner/project/pull/{{num}}#pullrequestreview-777",
                "state": state_map[payload["event"]],
                "commit_id": payload["commit_id"],
                "body": payload["body"],
                "user": {{"login": "stefan"}}
            }}))
            sys.exit(0)

sys.stderr.write(f"Unhandled gh call: {{args}}\n")
sys.exit(1)
"#,
            control = control.to_str().unwrap(),
            calls = gh_calls.to_str().unwrap(),
        );
        write_executable(&bin_dir.join("gh"), &gh_script)?;

        let editor = bin_dir.join("fake-editor");
        write_executable(&editor, "#!/bin/sh\nprintf 'LGTM from test\\n' > \"$1\"\n")?;

        Ok(Self {
            _temp: temp,
            root,
            bin_dir,
            gh_calls,
            editor,
            previous_commit,
        })
    }

    fn path_env(&self) -> String {
        let base = std::env::var("PATH").unwrap_or_default();
        format!("{}:{base}", self.bin_dir.display())
    }

    fn run_cli(&self, args: &[&str], stdin_data: &str, envs: &[(&str, &str)]) -> Result<Output> {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(&self.root)
            .env("PATH", self.path_env())
            .env("ZED_WORKTREE_ROOT", &self.root)
            .env_remove("ZED_FILE")
            .env_remove("ZED_ROW")
            .env_remove("ZED_COLUMN")
            .env_remove("ZED_SELECTED_TEXT")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().context("spawn zed-pr-review")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_data.as_bytes())?;
        }
        child.wait_with_output().context("wait zed-pr-review")
    }

    fn gh_call_count(&self) -> usize {
        fs::read_to_string(&self.gh_calls)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}

fn run_ok(cmd: &mut Command) -> Result<()> {
    let out = cmd.output()?;
    anyhow::ensure!(
        out.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn jj_output(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("jj")
        .current_dir(root)
        .env("JJ_PAGER", "cat")
        .args(["--no-pager", "--color=never"])
        .args(args)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "jj failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[test]
fn full_stack_open_navigate_comment_submit_badges_and_close() -> Result<()> {
    let fx = StackFixture::new()?;

    // 1. Open PR #101 — builds both review/pr-101 and review/pr-102, checks out review/pr-101.
    let open = fx.run_cli(&["--pr", "101", "open"], "", &[])?;
    assert!(
        open.status.success(),
        "open failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&open.stdout),
        String::from_utf8_lossy(&open.stderr)
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("one.txt"))?,
        "alpha\nbeta\n"
    );
    assert_eq!(fs::read_to_string(fx.root.join("two.txt"))?, "gamma\n");
    assert!(!fx.root.join("three.txt").exists());

    // 2. Comment locally on PR #101 (must make zero gh calls).
    let calls_before = fx.gh_call_count();
    let one_path = fx.root.join("one.txt");
    let c1 = fx.run_cli(
        &["comment"],
        "nit on alpha\n",
        &[
            ("ZED_FILE", one_path.to_str().unwrap()),
            ("ZED_ROW", "1"),
            ("ZED_COLUMN", "1"),
            ("ZED_SELECTED_TEXT", "alpha"),
        ],
    )?;
    assert!(
        c1.status.success(),
        "comment 1 failed: {}",
        String::from_utf8_lossy(&c1.stderr)
    );
    assert_eq!(fx.gh_call_count(), calls_before, "comment must be offline");

    // 3. Move up the stack with `sf`, comment locally on PR #102.
    let sf = fx.run_cli(&["sf"], "", &[])?;
    assert!(
        sf.status.success(),
        "sf failed: {}",
        String::from_utf8_lossy(&sf.stderr)
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("three.txt"))?,
        "delta\nepsilon\n"
    );
    let three_path = fx.root.join("three.txt");
    let c2 = fx.run_cli(
        &["comment"],
        "comment on delta\n",
        &[
            ("ZED_FILE", three_path.to_str().unwrap()),
            ("ZED_ROW", "1"),
            ("ZED_COLUMN", "1"),
            ("ZED_SELECTED_TEXT", "delta"),
        ],
    )?;
    assert!(
        c2.status.success(),
        "comment 2 failed: {}",
        String::from_utf8_lossy(&c2.stderr)
    );

    // 4. Move back down with `sb`, add a second comment on PR #101.
    let sb = fx.run_cli(&["sb"], "", &[])?;
    assert!(sb.status.success());
    let two_path = fx.root.join("two.txt");
    let c3 = fx.run_cli(
        &["comment"],
        "second comment on pr101\n",
        &[
            ("ZED_FILE", two_path.to_str().unwrap()),
            ("ZED_ROW", "1"),
            ("ZED_COLUMN", "1"),
            ("ZED_SELECTED_TEXT", "gamma"),
        ],
    )?;
    assert!(c3.status.success());

    // 5. Verify `log_data::load` attaches live pending comment badges.
    let snap = zed_pr_review::log_data::load(&fx.root)?;
    let rendered: String = snap
        .lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("[2 pending]"),
        "expected [2 pending] in:\n{rendered}"
    );
    assert!(
        rendered.contains("[1 pending]"),
        "expected [1 pending] in:\n{rendered}"
    );

    // 6. Approve PR #101 via `finish` (choice 3), verify it stays on the stack!
    let finish1 = fx.run_cli(
        &["--editor", fx.editor.to_str().unwrap(), "finish"],
        "3\n",
        &[],
    )?;
    assert!(
        finish1.status.success(),
        "finish 101 failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&finish1.stdout),
        String::from_utf8_lossy(&finish1.stderr)
    );
    let snap_after = zed_pr_review::log_data::load(&fx.root)?;
    let rendered_after: String = snap_after
        .lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered_after.contains("[✓ approved]"),
        "expected [✓ approved] in:\n{rendered_after}"
    );

    // 7. Move up to PR #102, abandon its draft via choice 1 (stays on stack), then close stack via choice 5.
    assert!(fx.run_cli(&["sf"], "", &[])?.status.success());
    let abandon = fx.run_cli(&["finish"], "1\n", &[])?;
    assert!(
        abandon.status.success(),
        "abandon draft failed: {}",
        String::from_utf8_lossy(&abandon.stderr)
    );
    let close = fx.run_cli(&["finish"], "5\n", &[])?;
    assert!(
        close.status.success(),
        "close via finish 5 failed: {}",
        String::from_utf8_lossy(&close.stderr)
    );
    let now = jj_output(
        &fx.root,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    )?;
    assert_eq!(now, fx.previous_commit);

    // Confirm commenting outside a review revision is rejected.
    let rejected = fx.run_cli(
        &["comment"],
        "nope\n",
        &[
            ("ZED_FILE", fx.root.join("base.txt").to_str().unwrap()),
            ("ZED_ROW", "1"),
            ("ZED_COLUMN", "1"),
            ("ZED_SELECTED_TEXT", "base"),
        ],
    )?;
    assert!(!rejected.status.success());

    Ok(())
}
