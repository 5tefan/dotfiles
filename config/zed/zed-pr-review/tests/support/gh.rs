//! Offline gh fixtures. No command in this module delegates to a real gh binary.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const REPO: &str = "acme/widgets";
const ACCEPT: &str = "Accept: application/vnd.github+json";
const VERSION: &str = "X-GitHub-Api-Version: 2022-11-28";
const READY_QUERY: &str = "mutation($id: ID!) {
    markPullRequestReadyForReview(input: {pullRequestId: $id}) {
        pullRequest { id isDraft }
    }
}";

pub fn run(config: &Value, args: Vec<OsString>) -> Result<i32> {
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let result = match config["tool"].as_str() {
        Some("gh") => run_stateful(args),
        Some("stack-gh") => run_stack(&args),
        Some("transport-gh") => run_transport(&args),
        _ => Err(anyhow::anyhow!("Unsupported gh fixture tool")),
    };
    match result {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("{error:#}");
            Ok(97)
        }
    }
}

fn env_path(name: &str) -> Result<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("Missing {name}"))
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Invalid JSON in {}", path.display()))
}

fn append_json(path: &Path, value: &Value) -> Result<()> {
    let mut record = serde_json::to_vec(value)?;
    record.push(b'\n');
    let mut stream = OpenOptions::new().create(true).append(true).open(path)?;
    stream.write_all(&record)?;
    Ok(())
}

fn print_json(value: &Value) -> Result<i32> {
    let mut stream = io::stdout().lock();
    serde_json::to_writer(&mut stream, value)?;
    writeln!(stream)?;
    Ok(0)
}

fn default(value: &Value, key: &str, fallback: Value) -> Value {
    value.get(key).cloned().unwrap_or(fallback)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn graphql_tokens(query: &str) -> Vec<String> {
    let mut chars = query.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() || c == ',' {
            continue;
        }
        let mut token = c.to_string();
        if c == '_' || c.is_ascii_alphabetic() {
            while let Some(c) = chars.next_if(|c| *c == '_' || c.is_ascii_alphanumeric()) {
                token.push(c);
            }
        }
        tokens.push(token);
    }
    tokens
}

struct Gh {
    args: Vec<String>,
    control_path: PathBuf,
    log_path: PathBuf,
    control: Value,
    record: Value,
    host: String,
    reviewer: Value,
    number: i64,
}

fn run_stateful(args: Vec<String>) -> Result<i32> {
    let mut gh = Gh {
        record: json!({
            "args": args,
            "cwd": std::env::current_dir()?,
            "git_dir": std::env::var_os("GIT_DIR").map(|v| v.to_string_lossy().into_owned()),
        }),
        args,
        control_path: env_path("ZPR_TEST_CONTROL")?,
        log_path: env_path("ZPR_TEST_GH_LOG")?,
        control: Value::Null,
        host: String::new(),
        reviewer: Value::Null,
        number: 17,
    };
    // Lock a stable sibling: saving control replaces its inode, and concurrent
    // requests must not share the temporary path or lose read/modify/write updates.
    let control_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(gh.control_path.with_extension("lock"))?;
    control_lock.lock()?;
    match gh.execute() {
        Ok(code) => Ok(code),
        Err(error) => {
            gh.record["unexpected"] = json!(format!("{error:#}"));
            gh.log()?;
            eprintln!("{error:#}");
            Ok(97)
        }
    }
}

impl Gh {
    fn log(&self) -> Result<()> {
        append_json(&self.log_path, &self.record)
    }

    fn emit(&self, value: Value) -> Result<i32> {
        self.log()?;
        print_json(&value)
    }

    fn save_control(&self) -> Result<()> {
        let temporary = self.control_path.with_extension("next");
        fs::write(&temporary, serde_json::to_vec(&self.control)?)?;
        fs::rename(temporary, &self.control_path)?;
        Ok(())
    }

    fn discovery_denied(&mut self) -> Result<bool> {
        if truthy(&self.control["deny_discovery"]) {
            self.record["discovery_denied"] = json!(true);
            self.log()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn url(&self, number: i64) -> String {
        format!("https://{}/{REPO}/pull/{number}", self.host)
    }

    fn apply_changes(&mut self, changes: Value) -> Result<()> {
        let mut changes = changes
            .as_object()
            .context("Expected fixture changes object")?
            .clone();
        if changes.remove("jj_new").is_some_and(|v| truthy(&v)) {
            let moved = jj_output(&["--no-pager", "new", "-m", "Checkout moved by fake gh"])?;
            ensure!(
                moved.status.success(),
                "Cannot move fixture checkout: {}",
                String::from_utf8_lossy(&moved.stderr)
            );
            let head = jj_output(&[
                "--no-pager",
                "--ignore-working-copy",
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ])?;
            ensure!(
                head.status.success(),
                "Cannot read moved fixture checkout: {}",
                String::from_utf8_lossy(&head.stderr)
            );
            self.control["jj_new_head"] = json!(String::from_utf8(head.stdout)?.trim());
        }
        self.control
            .as_object_mut()
            .context("Expected fixture control object")?
            .extend(changes);
        Ok(())
    }

    fn pr_control(&self, number: i64) -> Result<Value> {
        let mut values = self.control.clone();
        if let Some(overrides) = self
            .control
            .get("prs")
            .and_then(|prs| prs.get(number.to_string()))
        {
            values
                .as_object_mut()
                .context("Expected fixture control object")?
                .extend(
                    overrides
                        .as_object()
                        .context("Expected PR overrides object")?
                        .clone(),
                );
        }
        Ok(values)
    }

    fn pr_response(&self, number: i64) -> Result<Value> {
        let values = self.pr_control(number)?;
        let mut response = json!({
            "number": number,
            "html_url": self.url(number),
            "title": default(&values, "title", json!("Offline review fixture")),
            "state": default(&values, "pr_state", json!("open")),
            "merged": false, "merged_at": null,
            "user": {"login": default(&values, "author", json!("author"))},
            "head": {
                "sha": values.get("head").context("Missing fixture head")?,
                "ref": default(&values, "head_ref", json!("topic")),
                "repo": {"full_name": default(&values, "head_repo", json!(REPO))},
            },
            "base": {
                "sha": values.get("base").context("Missing fixture base")?,
                "ref": default(&values, "base_ref", json!("main")),
                "repo": {"full_name": REPO},
            },
        });
        if !truthy(&values["omit_draft"]) {
            response["draft"] = default(&values, "draft", json!(true));
        }
        if !truthy(&values["omit_node_id"]) {
            response["node_id"] = default(&values, "node_id", json!(format!("PR_fixture{number}")));
        }
        Ok(response)
    }

    fn mutate_ready(
        &mut self,
        kind: &str,
        key: &str,
        response: Value,
        requested: Option<String>,
    ) -> Result<i32> {
        let mode = default(&self.control, key, json!("success"));
        ensure!(
            matches!(mode.as_str(), Some("success" | "fail" | "lost_response")),
            "Unsupported mutation mode: {}",
            mode.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| mode.to_string())
        );
        self.record["mutation"] = json!(kind);
        if let Some(requested) = &requested {
            self.record["reviewer"] = json!(requested);
        }
        self.log()?;
        if mode == "fail" {
            eprintln!("simulated {kind} failure before acceptance");
            return Ok(1);
        }
        if kind == "ready" {
            self.control["draft"] = json!(false);
        } else {
            let reviewers = self
                .control
                .as_object_mut()
                .context("Expected fixture control object")?
                .entry("requested_reviewers")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("Expected requested_reviewers array")?;
            let requested = json!(requested.context("Missing requested reviewer")?);
            if !reviewers.contains(&requested) {
                reviewers.push(requested);
            }
        }
        self.apply_changes(default(&self.control, &format!("{key}_after"), json!({})))?;
        self.save_control()?;
        if mode == "lost_response" {
            eprintln!("connection lost after the server accepted {kind}");
            return Ok(1);
        }
        print_json(&response)
    }

    fn execute(&mut self) -> Result<i32> {
        self.control = read_json(&self.control_path)?;
        self.control
            .as_object()
            .context("Expected fixture control object")?;
        self.host = default(&self.control, "host", json!("github.com"))
            .as_str()
            .context("Expected host string")?
            .to_owned();
        self.reviewer = default(&self.control, "reviewer", json!("reviewer"));
        if self.args == ["repo", "view", "--json", "nameWithOwner,url"] {
            if self.discovery_denied()? {
                return Ok(3);
            }
            if let Some(changes) = self.control.get("repo_view_changes").cloned() {
                self.apply_changes(changes)?;
                self.save_control()?;
            }
            return self.emit(
                json!({"nameWithOwner": REPO, "url": format!("https://{}/{REPO}", self.host)}),
            );
        }
        if self.args.starts_with(&["pr".into(), "list".into()]) {
            let expected = vec![
                "pr".into(),
                "list".into(),
                "--repo".into(),
                format!("{}/{REPO}", self.host),
                "--head".into(),
                "topic".into(),
                "--state".into(),
                "open".into(),
                "--limit".into(),
                "100".into(),
                "--json".into(),
                "number,headRefOid,url,headRepository,headRepositoryOwner".into(),
            ];
            ensure!(self.args == expected, "Unexpected gh pr list arguments");
            if self.discovery_denied()? {
                return Ok(3);
            }
            if let Some(changes) = self.control.get("pr_list_changes").cloned() {
                self.apply_changes(changes)?;
                self.save_control()?;
            }
            if default(&self.control, "pr_state", json!("open")) != "open" {
                return self.emit(json!([]));
            }
            return self.emit(json!([{
                "number": 17, "headRefOid": self.control.get("head").context("Missing fixture head")?,
                "url": self.url(17), "headRepository": {"name": "widgets", "nameWithOwner": REPO},
                "headRepositoryOwner": {"login": "acme"},
            }]));
        }
        ensure!(
            self.args.first().is_some_and(|a| a == "api"),
            "Unsupported gh command"
        );
        let api = Api::parse(&self.args)?;
        ensure!(
            api.options.get("--hostname") == Some(&self.host),
            "API host must be explicit and match the discovered host"
        );
        ensure!(
            api.headers == BTreeSet::from([ACCEPT.to_owned(), VERSION.to_owned()]),
            "Missing REST API headers"
        );
        let method = api.options.get("--method").map(String::as_str);
        self.record["method"] = json!(method);
        self.record["endpoint"] = json!(api.endpoint);
        self.record["hostname"] = json!(self.host);
        let mut registered = BTreeSet::from([17]);
        if let Some(numbers) = self.control.get("additional_pr_numbers") {
            for number in numbers
                .as_array()
                .context("Expected additional_pr_numbers array")?
            {
                registered.insert(number.as_i64().context("Expected PR number")?);
            }
        }
        if let Some(prs) = self.control.get("prs") {
            for candidate in prs.as_object().context("Expected prs object")?.keys() {
                registered.insert(candidate.parse::<i64>()?);
            }
        }
        let endpoint = api.endpoint.as_deref().unwrap_or("");
        for candidate in &registered {
            let root = format!("repos/{REPO}/pulls/{candidate}");
            if endpoint == root || endpoint.starts_with(&format!("{root}/")) {
                self.number = *candidate;
                break;
            }
        }
        let endpoint_root = format!("repos/{REPO}/pulls/{}", self.number);
        if method == Some("POST") {
            ensure!(
                api.options.get("--input").map(String::as_str) == Some("-") && api.flags.is_empty(),
                "POST requires --input - and no pagination"
            );
            let payload: Value = serde_json::from_reader(io::stdin().lock())?;
            self.record["payload"] = payload.clone();
            return self.post(endpoint, &endpoint_root, payload);
        }
        ensure!(
            method == Some("GET") && !api.options.contains_key("--input"),
            "Expected an explicit GET without a request body"
        );
        if endpoint == "user" {
            ensure!(api.flags.is_empty(), "User endpoint is not paginated");
            return self.emit(json!({"login": self.reviewer}));
        }
        if endpoint == endpoint_root {
            if self.discovery_denied()? {
                return Ok(3);
            }
            ensure!(api.flags.is_empty(), "PR endpoint is not paginated");
            let count = self
                .control
                .get("pr_reads")
                .map_or(Ok(0), |v| v.as_u64().context("Expected PR read count"))?
                + 1;
            self.control["pr_reads"] = json!(count);
            self.record["pr_read"] = json!(count);
            let changes = self
                .control
                .get("pr_read_changes")
                .and_then(|v| v.get(count.to_string()))
                .cloned()
                .unwrap_or_else(|| json!({}));
            self.apply_changes(changes)?;
            self.save_control()?;
            return self.emit(self.pr_response(self.number)?);
        }
        ensure!(
            api.flags == BTreeSet::from(["--paginate".into(), "--slurp".into()]),
            "Collection requests must use --paginate --slurp"
        );
        if endpoint == format!("repos/{REPO}/pulls?state=open&per_page=100") {
            if self.discovery_denied()? {
                return Ok(3);
            }
            let pages = self
                .control
                .get("pulls_pages")
                .context("Open-pulls listing requires explicit pulls_pages control")?
                .as_array()
                .context("Expected pulls_pages array")?;
            let mut response = Vec::new();
            for page in pages {
                let mut values = Vec::new();
                for candidate in page.as_array().context("Expected pulls page array")? {
                    if candidate.is_object() {
                        values.push(candidate.clone());
                    } else if let Some(candidate) =
                        candidate.as_i64().filter(|n| registered.contains(n))
                    {
                        values.push(self.pr_response(candidate)?);
                    } else {
                        bail!("Unregistered PR in pulls_pages: {candidate}");
                    }
                }
                response.push(values);
            }
            return self.emit(json!(response));
        }
        if endpoint == format!("{endpoint_root}/files?per_page=100") {
            return self.emit(json!([default(
                &self.pr_control(self.number)?,
                "files",
                json!([{
                    "filename": "review file.txt", "status": "added",
                    "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three",
                }])
            )]));
        }
        if endpoint == format!("{endpoint_root}/reviews?per_page=100") {
            return self.emit(json!([default(&self.control, "reviews", json!([]))]));
        }
        if let Some(rest) = endpoint.strip_prefix(&format!("{endpoint_root}/reviews/"))
            && rest.ends_with("/comments?per_page=100")
        {
            let id = rest.split('/').next().unwrap();
            let comments = self
                .control
                .get("review_comments")
                .and_then(|v| v.get(id))
                .filter(|v| !v.is_null())
                .with_context(|| format!("No fixture comments for review {id}"))?;
            return self.emit(json!([comments]));
        }
        bail!("Unsupported API endpoint")
    }

    fn post(&mut self, endpoint: &str, endpoint_root: &str, payload: Value) -> Result<i32> {
        if endpoint == "graphql" {
            let node_id = default(&self.control, "node_id", json!("PR_fixture17"));
            ensure!(
                payload.as_object().is_some_and(|p| p.len() == 2
                    && p.contains_key("query")
                    && p.contains_key("variables"))
                    && payload["query"]
                        .as_str()
                        .is_some_and(|q| graphql_tokens(q) == graphql_tokens(READY_QUERY))
                    && payload["variables"] == json!({"id": node_id}),
                "Expected only markPullRequestReadyForReview for the REST node_id"
            );
            return self.mutate_ready(
                "ready",
                "ready",
                json!({"data": {"markPullRequestReadyForReview": {
                    "pullRequest": {"id": node_id, "isDraft": false},
                }}}),
                None,
            );
        }
        if endpoint == format!("{endpoint_root}/requested_reviewers") {
            let fields = payload
                .as_object()
                .context("Expected reviewers OR team_reviewers, without extra fields")?;
            ensure!(
                fields.len() == 1
                    && (fields.contains_key("reviewers") || fields.contains_key("team_reviewers")),
                "Expected reviewers OR team_reviewers, without extra fields"
            );
            let (field, values) = fields.iter().next().unwrap();
            let values = values
                .as_array()
                .context("Expected one reviewer login or unqualified team slug")?;
            let requested = values.first().and_then(Value::as_str).filter(|s| {
                s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
                    && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            });
            ensure!(
                values.len() == 1 && requested.is_some(),
                "Expected one reviewer login or unqualified team slug"
            );
            let requested = requested.unwrap();
            let requested = if field == "team_reviewers" {
                format!("acme/{requested}")
            } else {
                requested.to_owned()
            };
            return self.mutate_ready(
                "request-reviewer",
                "request_reviewer",
                json!({}),
                Some(requested),
            );
        }
        ensure!(
            endpoint == format!("{endpoint_root}/reviews"),
            "Unsupported POST endpoint"
        );
        let drafts = draft_paths(&env_path("ZPR_TEST_STATE")?)?;
        self.record["drafts_at_post"] = json!(
            drafts
                .iter()
                .map(|p| read_json(p))
                .collect::<Result<Vec<_>>>()?
        );
        if let Some(directories) = self.control.get("observe_sessions") {
            let mut observations = serde_json::Map::new();
            for directory in directories
                .as_array()
                .context("Expected observe_sessions array")?
            {
                let directory = directory
                    .as_str()
                    .context("Expected session directory string")?;
                let path = Path::new(directory);
                observations.insert(
                    directory.to_owned(),
                    json!({
                        "head": path.join("head").exists(),
                        "base": path.join("base").exists(),
                        "cleanup.json": path.join("cleanup.json").exists(),
                    }),
                );
            }
            self.record["sessions_at_post"] = Value::Object(observations);
        }
        self.log()?;
        let mode = default(&self.control, "post", json!("success"));
        if mode == "fail" {
            eprintln!("simulated connection failure");
            return Ok(1);
        }
        let state = match payload["event"].as_str() {
            Some("COMMENT") => "COMMENTED",
            Some("APPROVE") => "APPROVED",
            Some("REQUEST_CHANGES") => "CHANGES_REQUESTED",
            _ => bail!("Unsupported review event"),
        };
        let mut review = json!({
            "id": 501, "html_url": format!("{}#pullrequestreview-501", self.url(self.number)),
            "state": state, "body": payload.get("body").context("Missing review body")?,
            "commit_id": payload.get("commit_id").context("Missing review commit_id")?,
            "user": {"login": self.reviewer}, "submitted_at": "2026-01-01T00:00:00Z",
        });
        let mut comments = Vec::new();
        if let Some(submitted) = payload.get("comments") {
            for (index, comment) in submitted
                .as_array()
                .context("Expected review comments array")?
                .iter()
                .enumerate()
            {
                let mut saved = comment
                    .as_object()
                    .context("Expected review comment object")?
                    .clone();
                saved.extend(
                    json!({
                        "id": 1000 + index, "pull_request_review_id": 501,
                        "user": {"login": self.reviewer}, "commit_id": payload["commit_id"],
                        "original_commit_id": payload["commit_id"],
                        "original_line": comment.get("line").context("Missing comment line")?,
                        "original_start_line": comment["start_line"],
                        "start_line": comment["start_line"], "start_side": comment["start_side"],
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                );
                comments.push(Value::Object(saved));
            }
        }
        if mode == "delayed_acceptance" {
            self.control["delayed_review"] = review.clone();
            self.control["delayed_comments"] = json!(comments);
        } else {
            self.control
                .as_object_mut()
                .context("Expected fixture control object")?
                .entry("reviews")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("Expected reviews array")?
                .push(review.clone());
            self.control
                .as_object_mut()
                .unwrap()
                .entry("review_comments")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("Expected review_comments object")?
                .insert("501".into(), json!(comments));
        }
        self.save_control()?;
        if mode == "lost_response" || mode == "delayed_acceptance" {
            eprintln!("connection lost after the server accepted the review");
            return Ok(1);
        }
        if mode == "receipt_save_failure" {
            let matching = drafts
                .iter()
                .map(|path| Ok((path, read_json(path)?)))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, draft)| draft["pr"]["number"] == self.number)
                .map(|(path, _)| path)
                .collect::<Vec<_>>();
            ensure!(
                matching.len() == 1,
                "Expected one draft for receipt save failure: {matching:?}"
            );
            fs::hard_link(
                matching[0],
                matching[0].with_file_name("receipt-save-blocker"),
            )?;
        }
        if mode == "invalid_receipt" {
            review["body"] = json!("Response does not match the submitted review");
        }
        print_json(&review)
    }
}

struct Api {
    options: BTreeMap<String, String>,
    headers: BTreeSet<String>,
    flags: BTreeSet<String>,
    endpoint: Option<String>,
}

impl Api {
    fn parse(args: &[String]) -> Result<Self> {
        let mut api = Self {
            options: BTreeMap::new(),
            headers: BTreeSet::new(),
            flags: BTreeSet::new(),
            endpoint: None,
        };
        let mut args = args.iter().skip(1);
        while let Some(item) = args.next() {
            match item.as_str() {
                "--hostname" | "--method" | "--header" | "--input" => {
                    let value = args.next().context("Missing gh api option value")?;
                    if item == "--header" {
                        api.headers.insert(value.clone());
                    } else {
                        ensure!(
                            api.options.insert(item.clone(), value.clone()).is_none(),
                            "Duplicate gh api option"
                        );
                    }
                }
                "--paginate" | "--slurp" => {
                    api.flags.insert(item.clone());
                }
                item if !item.starts_with('-') && api.endpoint.is_none() => {
                    api.endpoint = Some(item.to_owned());
                }
                _ => bail!("Unsupported gh api argument: {item}"),
            }
        }
        Ok(api)
    }
}

fn draft_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path().join("draft.json");
        if path.exists() {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn jj_output(args: &[&str]) -> Result<std::process::Output> {
    let mut child = Command::new("jj")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Fixture jj command timed out after 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(child.wait_with_output()?)
}

fn run_stack(args: &[String]) -> Result<i32> {
    let root = env_path("ZPR_STACK_FIXTURE")?;
    let plan = read_json(&root.join("plan.json"))?;
    let index_path = root.join("index");
    let index = if index_path.exists() {
        fs::read_to_string(&index_path)?.trim().parse::<usize>()?
    } else {
        0
    };
    let step = plan["steps"]
        .as_array()
        .and_then(|steps| steps.get(index))
        .with_context(|| format!("Unexpected stack gh invocation after step {index}: {args:?}"))?;
    let host = plan["host"].as_str().context("Missing stack host")?;
    let endpoint = step["endpoint"]
        .as_str()
        .context("Missing stack endpoint")?;
    let mut expected = vec![
        "api",
        "--hostname",
        host,
        "--method",
        "GET",
        "--header",
        ACCEPT,
        "--header",
        VERSION,
        endpoint,
    ];
    if truthy(&step["pages"]) {
        expected.extend(["--paginate", "--slurp"]);
    }
    ensure!(
        args == expected,
        "Unexpected stack gh arguments at step {index}: {args:?}; expected {expected:?}"
    );
    ensure!(
        std::env::var("GH_HOST").ok().as_deref() == Some(host),
        "Stack GH_HOST must match the plan host"
    );
    ensure!(
        std::env::var_os("GH_REPO").is_none(),
        "Stack GH_REPO must be absent"
    );
    fs::write(index_path, (index + 1).to_string())?;
    print_json(step.get("body").context("Missing stack response body")?)
}

fn run_transport(args: &[String]) -> Result<i32> {
    let root = env_path("ZPR_FIXTURE")?;
    append_json(&root.join("gh-calls.jsonl"), &json!(args))?;
    if args == ["repo", "view", "--json", "nameWithOwner,url"] {
        ensure!(
            env_path("GIT_DIR")?.is_dir(),
            "Transport repo view requires GIT_DIR to be a directory"
        );
        return print_json(
            &json!({"nameWithOwner": "upstream/project", "url": "https://github.com/upstream/project"}),
        );
    }
    let method = args
        .iter()
        .position(|arg| arg == "--method")
        .and_then(|i| args.get(i + 1));
    ensure!(
        args.first().is_some_and(|a| a == "api") && method.is_some_and(|m| m == "GET"),
        "Unexpected transport gh arguments: {args:?}"
    );
    match args.last().map(String::as_str) {
        Some("user") => print_json(&json!({"login": "reviewer"})),
        Some("repos/upstream/project/pulls/42") => {
            println!("{}", fs::read_to_string(root.join("control.json"))?);
            Ok(0)
        }
        _ => bail!("Unexpected transport gh arguments: {args:?}"),
    }
}
