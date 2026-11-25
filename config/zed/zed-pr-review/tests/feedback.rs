//! End-to-end integration tests for the `zed-pr-review feedback` workflow with
//! real `jj`, a real bare Git `origin` remote, and a stateful fake `gh` CLI.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

struct FeedbackFixture {
    _temp: TempDir,
    root: PathBuf,
    bin_dir: PathBuf,
    gh_state: PathBuf,
}

impl FeedbackFixture {
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

        fs::write(root.join("base.rs"), "fn main() {}\n")?;
        jj_ok(&root, &["describe", "-m", "initial commit"])?;
        jj_ok(&root, &["bookmark", "create", "main", "-r", "@"])?;
        jj_ok(
            &root,
            &["git", "push", "--remote", "origin", "--bookmark", "main"],
        )?;
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

        // Stack revision 1: feat-a
        jj_ok(&root, &["new", "main", "-m", "feat a"])?;
        fs::write(root.join("a.rs"), "pub fn a() -> i32 {\n    1\n}\n")?;
        jj_ok(&root, &["bookmark", "create", "feat-a", "-r", "@"])?;
        let sha_a = jj_ok(&root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])?;
        jj_ok(
            &root,
            &["git", "push", "--remote", "origin", "--bookmark", "feat-a"],
        )?;

        // Stack revision 2: feat-b on top of feat-a
        jj_ok(&root, &["new", "feat-a", "-m", "feat b"])?;
        fs::write(root.join("b.rs"), "pub fn b() -> i32 {\n    2\n}\n")?;
        jj_ok(&root, &["bookmark", "create", "feat-b", "-r", "@"])?;
        let sha_b = jj_ok(&root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])?;
        jj_ok(
            &root,
            &["git", "push", "--remote", "origin", "--bookmark", "feat-b"],
        )?;

        let gh_state = temp.path().join("gh-state.json");
        let initial_state = json!({
            "user": "stefan",
            "prs": [
                {
                    "number": 101,
                    "node_id": "PR_101",
                    "state": "open",
                    "merged": false,
                    "draft": false,
                    "title": "feat a",
                    "html_url": format!("https://github.com/{REPO}/pull/101"),
                    "head": { "ref": "feat-a", "sha": sha_a, "repo": { "full_name": REPO } },
                    "base": { "ref": "main", "sha": "0000000000000000000000000000000000000000", "repo": { "full_name": REPO } },
                    "user": { "login": "stefan" }
                },
                {
                    "number": 102,
                    "node_id": "PR_102",
                    "state": "open",
                    "merged": false,
                    "draft": false,
                    "title": "feat b",
                    "html_url": format!("https://github.com/{REPO}/pull/102"),
                    "head": { "ref": "feat-b", "sha": sha_b, "repo": { "full_name": REPO } },
                    "base": { "ref": "feat-a", "sha": sha_a, "repo": { "full_name": REPO } },
                    "user": { "login": "stefan" }
                }
            ],
            "threads": {
                "101": [
                    {
                        "id": "PRRT_A1",
                        "isResolved": false,
                        "isOutdated": true,
                        "isCollapsed": false,
                        "path": "a.rs",
                        "line": null,
                        "originalLine": 2,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": [
                            {
                                "id": "PRRC_A1_1",
                                "databaseId": 10,
                                "author": { "login": "reviewer1" },
                                "body": "Please clarify return value",
                                "createdAt": "2026-01-01T00:00:00Z",
                                "url": format!("https://github.com/{REPO}/pull/101#discussion_r10")
                            }
                        ]
                    },
                    {
                        "id": "PRRT_A2_RESOLVED",
                        "isResolved": true,
                        "isOutdated": false,
                        "isCollapsed": true,
                        "path": "a.rs",
                        "line": 1,
                        "originalLine": 1,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": [
                            {
                                "id": "PRRC_A2_1",
                                "databaseId": 11,
                                "author": { "login": "reviewer1" },
                                "body": "Already done",
                                "createdAt": "2026-01-01T00:00:00Z",
                                "url": format!("https://github.com/{REPO}/pull/101#discussion_r11")
                            }
                        ]
                    }
                ],
                "102": [
                    {
                        "id": "PRRT_B1",
                        "isResolved": false,
                        "isOutdated": false,
                        "isCollapsed": false,
                        "path": "b.rs",
                        "line": 2,
                        "originalLine": 2,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": [
                            {
                                "id": "PRRC_B1_1",
                                "databaseId": 20,
                                "author": { "login": "reviewer2" },
                                "body": "Extract helper here",
                                "createdAt": "2026-01-02T00:00:00Z",
                                "url": format!("https://github.com/{REPO}/pull/102#discussion_r20")
                            }
                        ]
                    }
                ]
            },
            "fail_resolve_once": false,
            "next_comment_id": 100
        });
        fs::write(&gh_state, serde_json::to_vec_pretty(&initial_state)?)?;

        let gh_script = r#"#!/usr/bin/env python3
import json, os, sys, urllib.parse

state_path = os.environ["FAKE_GH_STATE"]
with open(state_path) as f:
    state = json.load(f)

args = sys.argv[1:]
if args[:2] == ["repo", "view"]:
    print(json.dumps({"nameWithOwner": "owner/project", "url": "https://github.com/owner/project"}))
    sys.exit(0)

if args[:1] == ["api"]:
    method = "GET"
    endpoint = None
    paginate = False
    i = 1
    while i < len(args):
        if args[i] == "--method":
            method = args[i+1]; i += 2
        elif args[i] in ("--hostname", "--header", "--input"):
            i += 2
        elif args[i] in ("--paginate", "--slurp"):
            paginate = True; i += 1
        else:
            endpoint = args[i]; i += 1

    if method == "GET" and endpoint == "user":
        print(json.dumps({"login": state["user"]}))
        sys.exit(0)

    if method == "GET" and endpoint.startswith("repos/owner/project/pulls?"):
        qs = urllib.parse.parse_qs(endpoint.split("?", 1)[1])
        head = qs.get("head", [""])[0]
        branch = head.split(":", 1)[1] if ":" in head else head
        matches = [p for p in state["prs"] if p["head"]["ref"] == branch]
        print(json.dumps([matches] if paginate else matches))
        sys.exit(0)

    if method == "GET" and endpoint.startswith("repos/owner/project/pulls/"):
        num = int(endpoint.rsplit("/", 1)[1])
        for p in state["prs"]:
            if p["number"] == num:
                print(json.dumps(p))
                sys.exit(0)
        sys.exit(1)

    if method == "POST" and endpoint == "graphql":
        payload = json.loads(sys.stdin.read())
        q = payload["query"]
        v = payload.get("variables", {})

        if "reviewThreads(" in q and "pullRequest(" in q:
            num = str(v["number"])
            pr = next(p for p in state["prs"] if str(p["number"]) == num)
            nodes = []
            for t in state["threads"].get(num, []):
                t_copy = dict(t)
                t_copy["comments"] = {
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                    "nodes": t["comments"],
                }
                nodes.append(t_copy)
            print(json.dumps({
                "data": {
                    "repository": {
                        "pullRequest": {
                            "headRefOid": pr["head"]["sha"],
                            "reviewThreads": {
                                "pageInfo": {"hasNextPage": False, "endCursor": None},
                                "nodes": nodes,
                            }
                        }
                    }
                }
            }))
            sys.exit(0)

        if "node(id:" in q:
            tid = v["id"]
            for num, tlist in state["threads"].items():
                for t in tlist:
                    if t["id"] == tid:
                        t_copy = dict(t)
                        t_copy["comments"] = {
                            "pageInfo": {"hasNextPage": False, "endCursor": None},
                            "nodes": t["comments"],
                        }
                        print(json.dumps({"data": {"node": t_copy}}))
                        sys.exit(0)
            print(json.dumps({"data": {"node": None}}))
            sys.exit(0)

        if "addPullRequestReviewThreadReply" in q:
            tid = v["threadId"]
            body = v["body"]
            cid_num = state["next_comment_id"]
            state["next_comment_id"] += 1
            new_c = {
                "id": f"PRRC_NEW_{cid_num}",
                "databaseId": cid_num,
                "author": {"login": state["user"]},
                "body": body,
                "createdAt": "2026-01-03T00:00:00Z",
                "url": f"https://github.com/owner/project/pull/101#discussion_r{cid_num}",
            }
            for tlist in state["threads"].values():
                for t in tlist:
                    if t["id"] == tid:
                        t["comments"].append(new_c)
            with open(state_path, "w") as f:
                json.dump(state, f)
            print(json.dumps({
                "data": {
                    "addPullRequestReviewThreadReply": {
                        "comment": new_c
                    }
                }
            }))
            sys.exit(0)

        if "resolveReviewThread" in q:
            if state.get("fail_resolve_once"):
                state["fail_resolve_once"] = False
                with open(state_path, "w") as f:
                    json.dump(state, f)
                print(json.dumps({"errors": [{"message": "transient network failure"}]}))
                sys.exit(0)
            tid = v["threadId"]
            for tlist in state["threads"].values():
                for t in tlist:
                    if t["id"] == tid:
                        t["isResolved"] = True
            with open(state_path, "w") as f:
                json.dump(state, f)
            print(json.dumps({
                "data": {
                    "resolveReviewThread": {
                        "thread": {"id": tid, "isResolved": True}
                    }
                }
            }))
            sys.exit(0)

sys.stderr.write(f"Unhandled gh args: {args}\n")
sys.exit(1)
"#;
        write_executable(&bin_dir.join("gh"), gh_script)?;

        Ok(Self {
            _temp: temp,
            root,
            bin_dir,
            gh_state,
        })
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let path_env = format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(BIN);
        cmd.current_dir(&self.root)
            .env("PATH", path_env)
            .env("FAKE_GH_STATE", &self.gh_state)
            .env("JJ_PAGER", "cat")
            .args(args);
        cmd
    }

    fn read_state(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&fs::read(&self.gh_state)?)?)
    }

    fn write_state(&self, value: &Value) -> Result<()> {
        fs::write(&self.gh_state, serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }
}

#[test]
fn feedback_list_draft_stale_detection_and_publish_flow() -> Result<()> {
    let fx = FeedbackFixture::new()?;

    // 1. `feedback list` shows unresolved threads grouped bottom-to-top (feat-a then feat-b),
    //    includes unresolved-but-outdated thread with [outdated], excludes resolved thread.
    let list_out = run_ok(&mut fx.cmd(&["feedback", "list"]))?;
    let stdout = String::from_utf8(list_out.stdout)?;
    assert!(stdout.contains("feat-a — PR #101"));
    assert!(stdout.contains("feat-b — PR #102"));
    assert!(stdout.find("feat-a").unwrap() < stdout.find("feat-b").unwrap());
    assert!(stdout.contains("[1] a.rs:2 (@reviewer1) [outdated]"));
    assert!(stdout.contains("[2] b.rs:2 (@reviewer2)"));
    assert!(!stdout.contains("Already done"));

    // `--all` includes resolved threads too.
    let all_out = run_ok(&mut fx.cmd(&["feedback", "list", "--all"]))?;
    let all_stdout = String::from_utf8(all_out.stdout)?;
    assert!(all_stdout.contains("[resolved]"));
    assert!(all_stdout.contains("Already done"));

    // 2. Stage replies and resolve flags offline across both revisions.
    run_ok(&mut fx.cmd(&[
        "feedback",
        "reply",
        "1",
        "--body",
        "Clarified in doc comment",
    ]))?;
    run_ok(&mut fx.cmd(&["feedback", "resolve", "1"]))?;
    run_ok(&mut fx.cmd(&["feedback", "reply", "2", "--body", "Extracted helper"]))?;
    run_ok(&mut fx.cmd(&["feedback", "resolve", "2"]))?;

    // 3. Simulate a reviewer posting a new comment on thread 1 before publish.
    let mut state = fx.read_state()?;
    state["threads"]["101"][0]["comments"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "PRRC_A1_NEW_HUMAN",
            "databaseId": 99,
            "author": { "login": "reviewer1" },
            "body": "Actually also handle negative numbers",
            "createdAt": "2026-01-02T12:00:00Z",
            "url": format!("https://github.com/{REPO}/pull/101#discussion_r99")
        }));
    fx.write_state(&state)?;

    // 4. Publish: thread 1 is skipped as stale; thread 2 is replied & resolved.
    let pub_out = run_ok(&mut fx.cmd(&["feedback", "publish", "--yes"]))?;
    let pub_stdout = String::from_utf8(pub_out.stdout)?;
    let pub_stderr = String::from_utf8(pub_out.stderr)?;
    assert!(pub_stderr.contains("skipping PR #101 a.rs:2"));
    assert!(pub_stdout.contains("1 reply posted, 1 thread resolved, 1 skipped"));

    let after = fx.read_state()?;
    // Thread A1 was untouched (still 2 comments, still unresolved).
    assert_eq!(
        after["threads"]["101"][0]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(after["threads"]["101"][0]["isResolved"], false);
    // Thread B1 received the reply and was resolved.
    assert_eq!(
        after["threads"]["102"][0]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        after["threads"]["102"][0]["comments"][1]["body"],
        "Extracted helper"
    );
    assert_eq!(after["threads"]["102"][0]["isResolved"], true);

    Ok(())
}

#[test]
fn unpushed_guard_and_partial_failure_recovery() -> Result<()> {
    let fx = FeedbackFixture::new()?;

    run_ok(&mut fx.cmd(&["feedback", "fetch"]))?;
    run_ok(&mut fx.cmd(&["feedback", "reply", "2", "--body", "Fixed in latest commit"]))?;
    run_ok(&mut fx.cmd(&["feedback", "resolve", "2"]))?;

    // Edit feat-b locally without pushing -> publish must refuse without --allow-unpushed.
    fs::write(fx.root.join("b.rs"), "pub fn b() -> i32 {\n    42\n}\n")?;
    let refused = fx.cmd(&["feedback", "publish", "--yes"]).output()?;
    assert!(!refused.status.success());
    let refused_err = String::from_utf8_lossy(&refused.stderr);
    assert!(refused_err.contains("Local bookmarks differ from GitHub PR heads"));

    // Inject transient resolve failure after reply succeeds.
    let mut state = fx.read_state()?;
    state["fail_resolve_once"] = json!(true);
    fx.write_state(&state)?;

    let failed = fx
        .cmd(&["feedback", "publish", "--allow-unpushed", "--yes"])
        .output()?;
    assert!(!failed.status.success());

    // Reply landed on GitHub, resolve did not.
    let mid = fx.read_state()?;
    assert_eq!(
        mid["threads"]["102"][0]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(mid["threads"]["102"][0]["isResolved"], false);

    // Recover with --retry (confirming "yes" on stdin) enables retrying ONLY the resolve step.
    let mut recover_child = fx
        .cmd(&["feedback", "recover", "--retry"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    recover_child.stdin.as_mut().unwrap().write_all(b"yes\n")?;
    let recover_out = recover_child.wait_with_output()?;
    assert!(recover_out.status.success());

    // Re-running publish resolves the thread WITHOUT posting a duplicate reply.
    run_ok(&mut fx.cmd(&["feedback", "publish", "--allow-unpushed", "--yes"]))?;
    let final_state = fx.read_state()?;
    assert_eq!(
        final_state["threads"]["102"][0]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "Reply must not be duplicated after partial-failure recovery"
    );
    assert_eq!(final_state["threads"]["102"][0]["isResolved"], true);

    Ok(())
}

#[test]
fn finish_on_own_stack_uses_feedback_menu_without_approve_or_reject() -> Result<()> {
    let fx = FeedbackFixture::new()?;
    run_ok(&mut fx.cmd(&["feedback", "fetch"]))?;
    run_ok(&mut fx.cmd(&["feedback", "reply", "2", "--body", "Done"]))?;

    let mut child = fx
        .cmd(&["finish"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.as_mut().unwrap().write_all(b"1\nyes\n")?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("Reply & publish feedback"));
    assert!(!stdout.contains("Approve"));
    assert!(!stdout.contains("Request changes"));
    assert!(stdout.contains("1 reply posted"));

    let state = fx.read_state()?;
    assert_eq!(
        state["threads"]["102"][0]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}

#[test]
fn agent_feedback_reply_by_thread_id_and_json_draft_state_and_publish_quoting() -> Result<()> {
    let fx = FeedbackFixture::new()?;

    // Fetch threads and inspect JSON output.
    run_ok(&mut fx.cmd(&["feedback", "fetch"]))?;
    let list_before = run_ok(&mut fx.cmd(&["feedback", "list", "--cached", "--json"]))?;
    let before_json: Value = serde_json::from_slice(&list_before.stdout)?;
    let items = before_json.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0]["staged_reply"].is_null());
    assert_eq!(items[0]["staged_reply_agent"], false);
    assert!(
        items[0]["change_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    // Stage an agent reply targeting PR #102 by `--thread` ID and `--body-file`.
    let body_file = fx._temp.path().join("agent_reply.md");
    fs::write(
        &body_file,
        "Explained the edge case.\n\n```rust\nfn b() -> i32 { 2 }\n```\n",
    )?;
    let reply_out = run_ok(&mut fx.cmd(&[
        "feedback",
        "reply",
        "--pr",
        "102",
        "--thread",
        "PRRT_B1",
        "--agent",
        "--body-file",
        body_file.to_str().unwrap(),
    ]))?;
    let reply_stdout = String::from_utf8(reply_out.stdout)?;
    assert!(reply_stdout.contains("[agent]"));

    // `feedback list --cached --json` shows raw unquoted staged reply + `staged_reply_agent: true`.
    let list_after = run_ok(&mut fx.cmd(&["feedback", "list", "--cached", "--json"]))?;
    let after_json: Value = serde_json::from_slice(&list_after.stdout)?;
    let item102 = after_json
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["pr_number"] == 102)
        .unwrap();
    assert_eq!(
        item102["staged_reply"],
        "Explained the edge case.\n\n```rust\nfn b() -> i32 { 2 }\n```"
    );
    assert_eq!(item102["staged_reply_agent"], true);
    assert_eq!(item102["staged_resolve"], false);

    // Publishing renders the agent reply as a blockquote signed `\- an agent` on GitHub.
    run_ok(&mut fx.cmd(&["feedback", "publish", "--yes"]))?;
    let state = fx.read_state()?;
    let comments = state["threads"]["102"][0]["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(
        comments[1]["body"],
        "> Explained the edge case.\n>\n> ```rust\n> fn b() -> i32 { 2 }\n> ```\n\n\\- an agent"
    );

    Ok(())
}
