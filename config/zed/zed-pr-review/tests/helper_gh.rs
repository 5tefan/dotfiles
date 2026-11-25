#![cfg(unix)]
//! Direct subprocess contracts for the offline gh helper, without real GitHub access.

mod support;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const ROOT: &str = "repos/acme/widgets/pulls/17";
const READY: &str = "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { id isDraft } } }";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    path: OsString,
}

impl Fixture {
    fn new(tool: &str) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("zpr-helper-gh-")
            .tempdir()?;
        let root = temp.path().canonicalize()?;
        for name in ["bin", "home", "state", "git-dir"] {
            fs::create_dir(root.join(name))?;
        }
        support::install(&root.join("bin/gh"), &json!({"tool": tool}))?;
        let fixture = Self {
            path: root.join("bin").into_os_string(),
            _temp: temp,
            root,
        };
        fixture.set_control(json!({"head": "a".repeat(40), "base": "b".repeat(40)}))?;
        Ok(fixture)
    }

    fn set_control(&self, value: Value) -> Result<()> {
        fs::write(self.root.join("control.json"), serde_json::to_vec(&value)?)?;
        Ok(())
    }

    fn control(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&fs::read(
            self.root.join("control.json"),
        )?)?)
    }

    fn configure(&self, key: &str, value: Value) -> Result<()> {
        let mut control = self.control()?;
        control[key] = value;
        self.set_control(control)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.root.join("bin/gh"));
        command
            .env_clear()
            .current_dir(&self.root)
            .env("PATH", &self.path)
            .env("HOME", self.root.join("home"))
            .env("GIT_DIR", self.root.join("git-dir"))
            .env("ZPR_TEST_CONTROL", self.root.join("control.json"))
            .env("ZPR_TEST_GH_LOG", self.root.join("gh.jsonl"))
            .env("ZPR_TEST_STATE", self.root.join("state"))
            .env("ZPR_STACK_FIXTURE", &self.root)
            .env("ZPR_FIXTURE", &self.root);
        command
    }

    fn invoke(&self, args: &[String], payload: Option<&Value>) -> Result<Output> {
        let mut command = self.command();
        command.args(args);
        execute(&mut command, payload)
    }

    fn api(
        &self,
        method: &str,
        endpoint: &str,
        pages: bool,
        payload: Option<&Value>,
    ) -> Result<Output> {
        self.invoke(
            &api_args(method, endpoint, pages, payload.is_some()),
            payload,
        )
    }

    fn calls(&self) -> Result<Vec<Value>> {
        json_lines(&self.root.join("gh.jsonl"))
    }
}

fn execute(command: &mut Command, payload: Option<&Value>) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(payload) = payload {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(payload)?)?;
    } else {
        drop(child.stdin.take());
    }
    Ok(child.wait_with_output()?)
}

fn api_args(method: &str, endpoint: &str, pages: bool, input: bool) -> Vec<String> {
    let mut args = vec![
        "api",
        "--hostname",
        "github.com",
        "--method",
        method,
        "--header",
        "Accept: application/vnd.github+json",
        "--header",
        "X-GitHub-Api-Version: 2022-11-28",
        endpoint,
    ];
    if pages {
        args.extend(["--paginate", "--slurp"]);
    }
    if input {
        args.extend(["--input", "-"]);
    }
    args.into_iter().map(str::to_owned).collect()
}

fn json_lines(path: &std::path::Path) -> Result<Vec<Value>> {
    fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn value(output: Output) -> Result<Value> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn ready_payload() -> Value {
    json!({"query": READY, "variables": {"id": "PR_fixture17"}})
}

fn review_payload() -> Value {
    json!({
        "event": "COMMENT", "body": "Review summary", "commit_id": "a".repeat(40),
        "comments": [
            {"path": "review file.txt", "line": 3, "side": "RIGHT", "body": "Range", "start_line": 1, "start_side": "RIGHT"},
            {"path": "review file.txt", "line": 2, "side": "RIGHT", "body": "Single"},
        ],
    })
}

#[test]
fn concurrent_pr_reads_preserve_every_update_and_log_record() -> Result<()> {
    struct Children(Vec<Child>);

    impl Drop for Children {
        fn drop(&mut self) {
            for child in &mut self.0 {
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }

    const COUNT: usize = 12;
    let fixture = Fixture::new("gh")?;
    let args = api_args("GET", ROOT, false, false);
    let mut children = Children(Vec::new());
    for index in 0..COUNT {
        children.0.push(
            fixture
                .command()
                .args(&args)
                .stdin(Stdio::null())
                .stdout(fs::File::create(
                    fixture.root.join(format!("pr-{index}.json")),
                )?)
                .stderr(fs::File::create(
                    fixture.root.join(format!("pr-{index}.stderr")),
                )?)
                .spawn()?,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    for (index, child) in children.0.iter_mut().enumerate() {
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "Concurrent PR read {index} timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        ensure!(
            status.success(),
            "Concurrent PR read {index} failed: {}",
            fs::read_to_string(fixture.root.join(format!("pr-{index}.stderr")))?
        );
        let response: Value =
            serde_json::from_slice(&fs::read(fixture.root.join(format!("pr-{index}.json")))?)?;
        assert_eq!(response["number"], 17);
    }
    assert_eq!(fixture.control()?["pr_reads"], COUNT);
    let calls = fixture.calls()?;
    assert_eq!(calls.len(), COUNT);
    let mut reads = Vec::new();
    for call in calls {
        assert!(call.get("unexpected").is_none(), "{call}");
        assert_eq!(call["args"], json!(args));
        reads.push(call["pr_read"].as_u64().context("Missing PR read count")?);
    }
    reads.sort_unstable();
    assert_eq!(reads, (1..=COUNT as u64).collect::<Vec<_>>());
    assert!(!fixture.root.join("control.next").exists());
    Ok(())
}

#[test]
fn rejects_unexpected_commands_headers_hosts_and_options_without_fallback() -> Result<()> {
    let fixture = Fixture::new("gh")?;
    let valid = api_args("GET", "user", false, false);
    let mut cases = vec![
        (
            vec!["auth".into(), "status".into()],
            "Unsupported gh command",
        ),
        (
            vec!["pr".into(), "list".into()],
            "Unexpected gh pr list arguments",
        ),
    ];
    let mut wrong_host = valid.clone();
    wrong_host[2] = "elsewhere.test".into();
    cases.push((wrong_host, "API host must be explicit"));
    let mut absent_host = valid.clone();
    absent_host.drain(1..3);
    cases.push((absent_host, "API host must be explicit"));
    let mut missing_header = valid.clone();
    missing_header.drain(5..7);
    cases.push((missing_header, "Missing REST API headers"));
    let mut wrong_header = valid.clone();
    wrong_header[6] = "Accept: application/json".into();
    cases.push((wrong_header, "Missing REST API headers"));
    let mut extra_header = valid.clone();
    extra_header.extend(["--header".into(), "Authorization: no".into()]);
    cases.push((extra_header, "Missing REST API headers"));
    let mut duplicate = valid.clone();
    duplicate.extend(["--method".into(), "GET".into()]);
    cases.push((duplicate, "Duplicate gh api option"));
    let mut missing = valid.clone();
    missing.push("--input".into());
    cases.push((missing, "Missing gh api option value"));
    let mut unknown = valid.clone();
    unknown.push("--silent".into());
    cases.push((unknown, "Unsupported gh api argument"));
    let mut extra_endpoint = valid;
    extra_endpoint.push("extra".into());
    cases.push((extra_endpoint, "Unsupported gh api argument"));
    cases.push((
        api_args("GET", "user", true, false),
        "User endpoint is not paginated",
    ));
    cases.push((
        api_args("GET", ROOT, true, false),
        "PR endpoint is not paginated",
    ));
    cases.push((
        api_args("GET", &format!("{ROOT}/files?per_page=100"), false, false),
        "Collection requests must use",
    ));
    cases.push((
        api_args("PATCH", ROOT, false, false),
        "Expected an explicit GET",
    ));
    cases.push((
        api_args("GET", ROOT, false, true),
        "Expected an explicit GET",
    ));
    cases.push((
        api_args("POST", "graphql", false, false),
        "POST requires --input",
    ));
    cases.push((
        api_args("POST", "graphql", true, true),
        "POST requires --input",
    ));
    cases.push((
        api_args("GET", "unknown", true, false),
        "Unsupported API endpoint",
    ));
    for (args, message) in &cases {
        let output = fixture.invoke(args, None)?;
        assert_eq!(output.status.code(), Some(97), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
    }
    let calls = fixture.calls()?;
    assert_eq!(calls.len(), cases.len());
    for (call, (args, message)) in calls.iter().zip(cases) {
        assert_eq!(call["args"], json!(args));
        assert!(call["unexpected"].as_str().unwrap().contains(message));
        assert_eq!(call["cwd"], json!(fixture.root));
        assert_eq!(call["git_dir"], json!(fixture.root.join("git-dir")));
    }
    Ok(())
}

#[test]
fn headers_and_flags_use_sets_but_option_names_remain_strict() -> Result<()> {
    let fixture = Fixture::new("gh")?;
    let mut args = api_args("GET", &format!("{ROOT}/files?per_page=100"), true, false);
    args.extend([
        "--header".into(),
        "Accept: application/vnd.github+json".into(),
        "--paginate".into(),
    ]);
    let response = value(fixture.invoke(&args, None)?)?;
    assert_eq!(
        response[0][0],
        json!({
            "filename": "review file.txt", "status": "added", "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three",
        })
    );
    let mut payload = ready_payload();
    payload["query"] = json!(READY.replace(' ', ",\n\t"));
    assert_eq!(
        value(fixture.api("POST", "graphql", false, Some(&payload))?)?["data"]["markPullRequestReadyForReview"]
            ["pullRequest"]["isDraft"],
        false
    );
    for query in [
        READY.replace("isDraft", "is Draft"),
        format!("{READY} query {{ viewer {{ login }} }}"),
    ] {
        payload["query"] = json!(query);
        assert_eq!(
            fixture
                .api("POST", "graphql", false, Some(&payload))?
                .status
                .code(),
            Some(97)
        );
    }
    let mut payload = ready_payload();
    payload["extra"] = json!(true);
    assert_eq!(
        fixture
            .api("POST", "graphql", false, Some(&payload))?
            .status
            .code(),
        Some(97)
    );
    payload = ready_payload();
    payload["variables"]["id"] = json!("wrong");
    assert_eq!(
        fixture
            .api("POST", "graphql", false, Some(&payload))?
            .status
            .code(),
        Some(97)
    );
    Ok(())
}

#[test]
fn discovery_patches_pr_overrides_pages_and_malformed_outputs_are_preserved() -> Result<()> {
    let fixture = Fixture::new("gh")?;
    fixture.configure("host", json!("github.example.test"))?;
    fixture.configure("repo_view_changes", json!({"title": "After discovery"}))?;
    let repo = value(fixture.invoke(
        &["repo", "view", "--json", "nameWithOwner,url"].map(str::to_owned),
        None,
    )?)?;
    assert_eq!(repo["url"], "https://github.example.test/acme/widgets");
    assert_eq!(fixture.control()?["title"], "After discovery");
    fixture.configure("pr_list_changes", json!({"head": "c".repeat(40)}))?;
    let list_args = [
        "pr",
        "list",
        "--repo",
        "github.example.test/acme/widgets",
        "--head",
        "topic",
        "--state",
        "open",
        "--limit",
        "100",
        "--json",
        "number,headRefOid,url,headRepository,headRepositoryOwner",
    ]
    .map(str::to_owned);
    let list = value(fixture.invoke(&list_args, None)?)?;
    assert_eq!(list[0]["headRefOid"], "c".repeat(40));
    fixture.configure("pr_state", json!("closed"))?;
    assert_eq!(value(fixture.invoke(&list_args, None)?)?, json!([]));
    fixture.configure("host", json!("github.com"))?;
    fixture.configure("pr_state", json!("open"))?;
    fixture.configure(
        "pr_read_changes",
        json!({"1": {"draft": false, "base_ref": "develop"}, "2": {"title": "Second read"}}),
    )?;
    fixture.configure("additional_pr_numbers", json!([19]))?;
    fixture.configure("prs", json!({"23": {"head_ref": "other", "head_repo": "fork/widgets", "title": null, "omit_draft": true, "omit_node_id": true, "files": null}}))?;
    let first = value(fixture.api("GET", ROOT, false, None)?)?;
    assert_eq!(first["draft"], false);
    assert_eq!(first["base"]["ref"], "develop");
    assert_eq!(first["node_id"], "PR_fixture17");
    let second = value(fixture.api("GET", "repos/acme/widgets/pulls/23", false, None)?)?;
    assert!(second.get("draft").is_none());
    assert!(second.get("node_id").is_none());
    assert!(second["title"].is_null());
    assert_eq!(second["head"]["repo"]["full_name"], "fork/widgets");
    assert_eq!(fixture.control()?["pr_reads"], 2);
    assert_eq!(fixture.control()?["title"], "Second read");
    assert_eq!(
        value(fixture.api(
            "GET",
            "repos/acme/widgets/pulls/23/files?per_page=100",
            true,
            None
        )?)?,
        json!([null])
    );
    fixture.configure("pulls_pages", json!([[23, {"malformed": true}], [19, 17]]))?;
    let pages = value(fixture.api(
        "GET",
        "repos/acme/widgets/pulls?state=open&per_page=100",
        true,
        None,
    )?)?;
    assert_eq!(pages[0][0], second);
    assert_eq!(pages[0][1], json!({"malformed": true}));
    assert_eq!(pages[1][0]["node_id"], "PR_fixture19");
    assert_eq!(pages[1][1]["title"], "Second read");
    fixture.configure("pulls_pages", json!([[99]]))?;
    assert_eq!(
        fixture
            .api(
                "GET",
                "repos/acme/widgets/pulls?state=open&per_page=100",
                true,
                None
            )?
            .status
            .code(),
        Some(97)
    );
    fixture.configure("deny_discovery", json!(true))?;
    for args in [
        ["repo", "view", "--json", "nameWithOwner,url"]
            .map(str::to_owned)
            .to_vec(),
        api_args("GET", ROOT, false, false),
        api_args(
            "GET",
            "repos/acme/widgets/pulls?state=open&per_page=100",
            true,
            false,
        ),
    ] {
        assert_eq!(fixture.invoke(&args, None)?.status.code(), Some(3));
        assert_eq!(fixture.calls()?.last().unwrap()["discovery_denied"], true);
    }
    assert_eq!(fixture.control()?["pr_reads"], 2);
    Ok(())
}

#[test]
fn ready_and_reviewer_failures_distinguish_rejection_from_accepted_writes() -> Result<()> {
    for mode in ["success", "fail", "lost_response"] {
        let fixture = Fixture::new("gh")?;
        fixture.configure("ready", json!(mode))?;
        fixture.configure("draft", json!(true))?;
        fixture.configure("ready_after", json!({"base_ref": "moved"}))?;
        let output = fixture.api("POST", "graphql", false, Some(&ready_payload()))?;
        assert_eq!(
            output.status.code(),
            Some(if mode == "success" { 0 } else { 1 })
        );
        assert_eq!(fixture.control()?["draft"], mode == "fail");
        assert_eq!(
            fixture.control()?["base_ref"],
            if mode == "fail" {
                Value::Null
            } else {
                json!("moved")
            }
        );
        assert_eq!(fixture.calls()?[0]["mutation"], "ready");
        fixture.configure("request_reviewer", json!(mode))?;
        fixture.configure("requested_reviewers", json!(["already-there"]))?;
        fixture.configure("request_reviewer_after", json!({"title": "Requested"}))?;
        let payload = json!({"team_reviewers": ["Review-Team"]});
        for _ in 0..2 {
            let output = fixture.api(
                "POST",
                &format!("{ROOT}/requested_reviewers"),
                false,
                Some(&payload),
            )?;
            assert_eq!(
                output.status.code(),
                Some(if mode == "success" { 0 } else { 1 })
            );
        }
        let control = fixture.control()?;
        assert_eq!(
            control["requested_reviewers"],
            if mode == "fail" {
                json!(["already-there"])
            } else {
                json!(["already-there", "acme/Review-Team"])
            }
        );
        assert_eq!(
            control["title"],
            if mode == "fail" {
                Value::Null
            } else {
                json!("Requested")
            }
        );
        assert_eq!(fixture.calls()?[1]["reviewer"], "acme/Review-Team");
    }
    Ok(())
}

#[test]
fn reviewer_payloads_are_strict_and_invalid_mutation_modes_never_write() -> Result<()> {
    let fixture = Fixture::new("gh")?;
    let before = fixture.control()?;
    for payload in [
        json!([]),
        json!({}),
        json!({"reviewers": ["ok"], "team_reviewers": ["team"]}),
        json!({"reviewers": []}),
        json!({"reviewers": ["one", "two"]}),
        json!({"reviewers": [1]}),
        json!({"reviewers": ["-bad"]}),
        json!({"team_reviewers": ["acme/team"]}),
        json!({"reviewers": ["a_b"]}),
        json!({"reviewers": "login"}),
        json!({"reviewers": ["é"]}),
    ] {
        assert_eq!(
            fixture
                .api(
                    "POST",
                    &format!("{ROOT}/requested_reviewers"),
                    false,
                    Some(&payload)
                )?
                .status
                .code(),
            Some(97)
        );
        assert_eq!(fixture.control()?, before);
    }
    assert_eq!(
        value(fixture.api(
            "POST",
            &format!("{ROOT}/requested_reviewers"),
            false,
            Some(&json!({"reviewers": ["User-123"]}))
        )?)?,
        json!({})
    );
    assert_eq!(
        fixture.control()?["requested_reviewers"],
        json!(["User-123"])
    );
    fixture.configure("ready", json!("not-supported"))?;
    let before = fixture.control()?;
    assert_eq!(
        fixture
            .api("POST", "graphql", false, Some(&ready_payload()))?
            .status
            .code(),
        Some(97)
    );
    assert_eq!(fixture.control()?, before);
    Ok(())
}

#[test]
fn review_receipts_comments_observations_and_failure_modes_match_the_fixture() -> Result<()> {
    for mode in [
        "success",
        "fail",
        "lost_response",
        "delayed_acceptance",
        "invalid_receipt",
    ] {
        let fixture = Fixture::new("gh")?;
        fixture.configure("post", json!(mode))?;
        fixture.configure("reviewer", json!("custom-reviewer"))?;
        let session = fixture.root.join("state/session");
        fs::create_dir(&session)?;
        fs::create_dir(session.join("head"))?;
        fs::write(session.join("cleanup.json"), "{}")?;
        let draft = json!({"pr": {"number": 17}, "attempt": {"event": "COMMENT"}});
        fs::write(session.join("draft.json"), serde_json::to_vec(&draft)?)?;
        fixture.configure("observe_sessions", json!([session]))?;
        let payload = review_payload();
        let output = fixture.api("POST", &format!("{ROOT}/reviews"), false, Some(&payload))?;
        assert_eq!(
            output.status.code(),
            Some(if ["success", "invalid_receipt"].contains(&mode) {
                0
            } else {
                1
            })
        );
        let calls = fixture.calls()?;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["payload"], payload);
        assert_eq!(calls[0]["drafts_at_post"], json!([draft]));
        assert_eq!(
            calls[0]["sessions_at_post"][session.to_str().unwrap()],
            json!({"head": true, "base": false, "cleanup.json": true})
        );
        let control = fixture.control()?;
        if mode == "fail" {
            assert!(control.get("reviews").is_none());
            assert!(control.get("delayed_review").is_none());
            continue;
        }
        let (review, comments) = if mode == "delayed_acceptance" {
            assert!(control.get("reviews").is_none());
            assert_eq!(
                value(fixture.api("GET", &format!("{ROOT}/reviews?per_page=100"), true, None)?)?,
                json!([[]])
            );
            (&control["delayed_review"], &control["delayed_comments"])
        } else {
            assert_eq!(
                value(fixture.api("GET", &format!("{ROOT}/reviews?per_page=100"), true, None)?)?,
                json!([control["reviews"]])
            );
            assert_eq!(
                value(fixture.api(
                    "GET",
                    &format!("{ROOT}/reviews/501/comments?per_page=100"),
                    true,
                    None
                )?)?,
                json!([control["review_comments"]["501"]])
            );
            (&control["reviews"][0], &control["review_comments"]["501"])
        };
        assert_eq!(
            *review,
            json!({"id": 501, "html_url": "https://github.com/acme/widgets/pull/17#pullrequestreview-501", "state": "COMMENTED", "body": "Review summary", "commit_id": "a".repeat(40), "user": {"login": "custom-reviewer"}, "submitted_at": "2026-01-01T00:00:00Z"})
        );
        assert_eq!(comments[0]["id"], 1000);
        assert_eq!(comments[1]["id"], 1001);
        assert_eq!(comments[0]["original_line"], 3);
        assert_eq!(comments[0]["original_start_line"], 1);
        assert_eq!(comments[0]["start_side"], "RIGHT");
        assert_eq!(comments[0]["pull_request_review_id"], 501);
        assert_eq!(comments[0]["original_commit_id"], payload["commit_id"]);
        assert!(comments[1].get("start_line").is_some_and(Value::is_null));
        assert!(comments[1].get("start_side").is_some_and(Value::is_null));
        if output.status.success() {
            let receipt = value(output)?;
            assert_eq!(
                receipt["body"],
                if mode == "invalid_receipt" {
                    "Response does not match the submitted review"
                } else {
                    "Review summary"
                }
            );
        }
    }
    Ok(())
}

#[test]
fn receipt_save_failure_creates_a_hard_link_after_acceptance() -> Result<()> {
    let fixture = Fixture::new("gh")?;
    fixture.configure("post", json!("receipt_save_failure"))?;
    for (name, number) in [("selected", 17), ("other", 23)] {
        let session = fixture.root.join("state").join(name);
        fs::create_dir(&session)?;
        fs::write(
            session.join("draft.json"),
            serde_json::to_vec(&json!({"pr": {"number": number}}))?,
        )?;
    }
    for (event, state) in [
        ("APPROVE", "APPROVED"),
        ("REQUEST_CHANGES", "CHANGES_REQUESTED"),
    ] {
        let mut payload = review_payload();
        payload["event"] = json!(event);
        let receipt =
            value(fixture.api("POST", &format!("{ROOT}/reviews"), false, Some(&payload))?)?;
        assert_eq!(receipt["state"], state);
        let draft = fs::metadata(fixture.root.join("state/selected/draft.json"))?;
        let blocker = fs::metadata(fixture.root.join("state/selected/receipt-save-blocker"))?;
        assert_eq!(draft.ino(), blocker.ino());
        assert_eq!(draft.nlink(), 2);
        assert!(
            !fixture
                .root
                .join("state/other/receipt-save-blocker")
                .exists()
        );
        fs::remove_file(fixture.root.join("state/selected/receipt-save-blocker"))?;
    }
    assert_eq!(fixture.control()?["reviews"].as_array().unwrap().len(), 2);
    Ok(())
}

#[test]
fn stack_helper_requires_exact_order_host_and_clean_repository_environment() -> Result<()> {
    let fixture = Fixture::new("stack-gh")?;
    fs::write(
        fixture.root.join("plan.json"),
        serde_json::to_vec(&json!({
            "host": "github.com", "steps": [
                {"endpoint": "user", "body": {"login": "reviewer"}},
                {"endpoint": "collection", "pages": true, "body": [[{"invalid": true}]]},
            ],
        }))?,
    )?;
    let args = api_args("GET", "user", false, false);
    for (host, repo) in [
        ("wrong.test", None),
        ("github.com", Some("wrong/repository")),
    ] {
        let mut command = fixture.command();
        command.args(&args).env("GH_HOST", host);
        if let Some(repo) = repo {
            command.env("GH_REPO", repo);
        }
        assert_eq!(execute(&mut command, None)?.status.code(), Some(97));
        assert!(!fixture.root.join("index").exists());
    }
    let mut command = fixture.command();
    command.args(&args).env("GH_HOST", "github.com");
    assert_eq!(
        value(execute(&mut command, None)?)?,
        json!({"login": "reviewer"})
    );
    assert_eq!(fs::read_to_string(fixture.root.join("index"))?, "1");
    let mut command = fixture.command();
    command.args(&args).env("GH_HOST", "github.com");
    assert_eq!(execute(&mut command, None)?.status.code(), Some(97));
    assert_eq!(fs::read_to_string(fixture.root.join("index"))?, "1");
    let mut command = fixture.command();
    command
        .args(api_args("GET", "collection", true, false))
        .env("GH_HOST", "github.com");
    assert_eq!(
        value(execute(&mut command, None)?)?,
        json!([[{"invalid": true}]])
    );
    let mut command = fixture.command();
    command.args(&args).env("GH_HOST", "github.com");
    assert_eq!(execute(&mut command, None)?.status.code(), Some(97));
    assert_eq!(fs::read_to_string(fixture.root.join("index"))?, "2");
    Ok(())
}

#[test]
fn transport_helper_preserves_raw_control_output_and_logs_every_call() -> Result<()> {
    let fixture = Fixture::new("transport-gh")?;
    let repo_args = ["repo", "view", "--json", "nameWithOwner,url"].map(str::to_owned);
    assert_eq!(
        value(fixture.invoke(&repo_args, None)?)?,
        json!({"nameWithOwner": "upstream/project", "url": "https://github.com/upstream/project"})
    );
    let mut command = fixture.command();
    command.args(&repo_args).env_remove("GIT_DIR");
    assert_eq!(execute(&mut command, None)?.status.code(), Some(97));
    assert_eq!(
        value(fixture.api("GET", "user", false, None)?)?,
        json!({"login": "reviewer"})
    );
    fs::write(fixture.root.join("control.json"), "{ malformed JSON\n")?;
    let output = fixture.api("GET", "repos/upstream/project/pulls/42", false, None)?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{ malformed JSON\n\n");
    assert_eq!(
        fixture
            .api("GET", "unregistered", false, None)?
            .status
            .code(),
        Some(97)
    );
    assert_eq!(
        fixture
            .api("POST", "user", false, Some(&json!({})))?
            .status
            .code(),
        Some(97)
    );
    let calls = json_lines(&fixture.root.join("gh-calls.jsonl"))?;
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[0], json!(repo_args));
    assert!(calls.iter().all(Value::is_array));
    Ok(())
}

#[test]
fn checkout_changes_use_only_the_private_fixture_and_record_the_new_head() -> Result<()> {
    let mut fixture = Fixture::new("gh")?;
    let path = std::env::var_os("PATH").context("Missing PATH")?;
    let real_jj = std::env::split_paths(&path)
        .map(|p| p.join("jj"))
        .find(|p| p.is_file())
        .context("jj is required")?
        .canonicalize()?;
    let mut paths = vec![fixture.root.join("bin")];
    paths.extend(std::env::split_paths(&path));
    fixture.path = std::env::join_paths(paths)?;
    fs::write(
        fixture.root.join("jj.toml"),
        "[user]\nname = 'Helper Test'\nemail = 'helper@example.test'\n[ui]\npager = 'cat'\n",
    )?;
    let configure = |command: &mut Command| {
        command
            .env_remove("GIT_DIR")
            .env("JJ_CONFIG", fixture.root.join("jj.toml"))
            .env("JJ_PAGER", "cat")
            .env("RAYON_NUM_THREADS", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
    };
    let jj = |args: &[&str]| -> Result<String> {
        let mut command = Command::new(&real_jj);
        command
            .env_clear()
            .current_dir(&fixture.root)
            .env("PATH", &fixture.path)
            .env("HOME", fixture.root.join("home"));
        configure(&mut command);
        command.arg("--no-pager").args(args);
        let output = execute(&mut command, None)?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    jj(&["git", "init", "--no-colocate"])?;
    jj(&["describe", "-m", "Before fake gh"])?;
    let old = jj(&[
        "--ignore-working-copy",
        "log",
        "-r",
        "@",
        "--no-graph",
        "-T",
        "commit_id",
    ])?;
    fixture.configure(
        "repo_view_changes",
        json!({"jj_new": true, "title": "Moved"}),
    )?;
    let mut command = fixture.command();
    configure(&mut command);
    command.args(["repo", "view", "--json", "nameWithOwner,url"]);
    value(execute(&mut command, None)?)?;
    let new = jj(&[
        "--ignore-working-copy",
        "log",
        "-r",
        "@",
        "--no-graph",
        "-T",
        "commit_id",
    ])?;
    assert_ne!(new, old);
    assert_eq!(fixture.control()?["jj_new_head"], new);
    assert_eq!(fixture.control()?["title"], "Moved");
    assert!(fixture.control()?.get("jj_new").is_none());
    Ok(())
}
