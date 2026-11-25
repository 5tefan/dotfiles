use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub fn run(config: &Value, args: Vec<OsString>) -> Result<i32> {
    match field(config, "tool")? {
        "cli-git" | "cli-jj" => repository_tool(config, args),
        "editor" => editor(args),
        "zed" => zed(args),
        "gate" => gate(config, args),
        tool => bail!("Unknown CLI fixture tool: {tool}"),
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing string fixture setting: {key}"))
}

fn environment_path(name: &str) -> Result<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("Missing fixture environment variable: {name}"))
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Cannot read fixture JSON: {}", path.display()))
}

fn append_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&bytes)?;
    Ok(())
}

fn argument_strings(args: &[OsString]) -> Result<Vec<&str>> {
    args.iter()
        .map(|arg| arg.to_str().context("Fixture argument is not UTF-8"))
        .collect()
}

fn repository_tool(config: &Value, mut args: Vec<OsString>) -> Result<i32> {
    let arguments = argument_strings(&args)?;
    let has = |argument| arguments.contains(&argument);
    let mode = config["mode"].as_str().unwrap_or("safe");
    let program = config["program"].as_str().unwrap_or("git");
    match mode {
        "safe" => {
            ensure!(
                !["fetch", "push", "pull", "clone", "remote"]
                    .iter()
                    .any(|argument| has(argument)),
                "CLI fixture refused transport: {args:?}"
            );
        }
        "navigation" => {
            let root = Path::new(field(config, "root")?);
            append_json(
                &root.join("navigation.jsonl"),
                &json!({"program": program, "args": arguments, "cwd": std::env::current_dir()?}),
            )?;
            if program == "viddy" {
                eprintln!("Navigation must never invoke viddy");
                return Ok(97);
            }
            if has("fetch") {
                eprintln!("Navigation fixture refused network transport");
                return Ok(96);
            }
            ensure!(
                !has("remote") || (program == "git" && arguments.last() == Some(&"remote")),
                "Navigation fixture refused remote mutation: {args:?}"
            );
        }
        "local-pr" => {
            let root = Path::new(field(config, "root")?);
            append_json(
                &root.join("git.jsonl"),
                &json!({"args": arguments, "cwd": std::env::current_dir()?}),
            )?;
            if let Some(fetch) = arguments.iter().position(|argument| *argument == "fetch") {
                let separator = validate_fetch(config, &arguments, fetch)?;
                args[separator + 1] = root.join("remote.git").into_os_string();
                let mut command = Command::new(field(config, "real")?);
                command.args(args).env("GIT_ALLOW_PROTOCOL", "file");
                return bounded_status(command, Duration::from_secs(15));
            }
            // Bare `git remote` is a local read, not a remote mutation.
            ensure!(
                !has("remote") || arguments.last() == Some(&"remote"),
                "Local PR fixture refused remote mutation: {args:?}"
            );
        }
        _ => bail!("Unknown CLI repository fixture mode: {mode}"),
    }
    ensure!(
        !["push", "pull", "clone"]
            .iter()
            .any(|argument| has(argument)),
        "CLI fixture refused network transport: {args:?}"
    );
    ensure!(
        !has("ls-remote") || has("--get-url"),
        "CLI fixture refused remote read: {args:?}"
    );
    let error = Command::new(field(config, "real")?)
        .args(args)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .exec();
    Err(error).context("Cannot execute the real local repository tool")
}

fn validate_fetch(config: &Value, args: &[&str], fetch: usize) -> Result<usize> {
    let expected = "https://github.com/acme/widgets.git";
    ensure!(
        args.iter()
            .filter(|argument| **argument == expected)
            .count()
            == 1,
        "Expected exactly one offline PR URL: {args:?}"
    );
    let separator = args
        .iter()
        .enumerate()
        .skip(fetch)
        .find_map(|(index, argument)| (*argument == "--").then_some(index))
        .with_context(|| format!("Missing fetch operand separator: {args:?}"))?;
    let operands = &args[separator + 1..];
    ensure!(
        operands.len() == 3 && operands[0] == expected,
        "Expected a PR URL and exactly two refspecs: {args:?}"
    );
    let (source, destination) = operands[1]
        .split_once(':')
        .with_context(|| format!("Missing head refspec destination: {args:?}"))?;
    let number = source
        .strip_prefix("refs/pull/")
        .and_then(|source| source.strip_suffix("/head"))
        .filter(|number| {
            number.starts_with(|first: char| ('1'..='9').contains(&first))
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
        .with_context(|| format!("Expected an exact numbered PR head: {args:?}"))?;
    let namespace = destination
        .strip_suffix("/head")
        .with_context(|| format!("Expected a head destination: {args:?}"))?;
    let session = namespace
        .strip_prefix("refs/heads/zed-pr-review/")
        .is_some_and(|suffix| !suffix.is_empty() && !suffix.contains(['/', ':']));
    let prefetch = namespace
        .strip_prefix("refs/zed-pr-review-prefetch/")
        .is_some_and(|suffix| {
            suffix.len() == 64
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    ensure!(session || prefetch, "Unexpected PR ref namespace: {args:?}");
    let base = config["bases"][number]
        .as_str()
        .with_context(|| format!("PR {number} is not served by this offline fixture"))?;
    ensure!(
        operands[2] == format!("refs/heads/{base}:{namespace}/base"),
        "Expected the corresponding full base refspec: {args:?}"
    );
    ensure!(
        !args.iter().any(|argument| argument.starts_with('+')
            || argument.starts_with("--depth")
            || argument.starts_with("--shallow")),
        "Forced or shallow fetch is forbidden: {args:?}"
    );
    for flag in [
        "--atomic",
        "--no-tags",
        "--no-prune",
        "--no-prune-tags",
        "--no-recurse-submodules",
        "--no-auto-maintenance",
        "--no-write-commit-graph",
        "--no-write-fetch-head",
        "--refmap=",
        "--no-filter",
    ] {
        ensure!(args.contains(&flag), "Fetch is missing {flag}: {args:?}");
    }
    ensure!(
        std::env::var("GIT_ALLOW_PROTOCOL").as_deref() == Ok("https"),
        "The production fetch must allow only HTTPS"
    );
    Ok(separator)
}

fn bounded_status(mut command: Command, timeout: Duration) -> Result<i32> {
    let mut child = command.spawn().context("Cannot start offline Git fetch")?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                result.context("Cannot wait for offline Git fetch")?;
                bail!("Offline Git fetch exceeded {} seconds", timeout.as_secs());
            }
        }
    }
}

fn editor(args: Vec<OsString>) -> Result<i32> {
    super::review_terminal::editor_handoff()?;
    let control_path = environment_path("ZPR_TEST_CONTROL")?;
    let mut control = read_json(&control_path)?;
    let (path, arguments) = args.split_last().context("Editor needs a file argument")?;
    let path = Path::new(path);
    let initial = fs::read_to_string(path)?;
    append_json(
        &environment_path("ZPR_TEST_EDITOR_LOG")?,
        &json!({"args": argument_strings(arguments)?, "path": path, "initial": initial}),
    )?;
    let mode = control["editor_mode"]
        .as_str()
        .unwrap_or("write")
        .to_owned();
    let body = control["editor_body"].as_str().unwrap_or("Review summary");
    match mode.as_str() {
        "prepend" => fs::write(path, format!("{body}{initial}"))?,
        "unchanged" => (),
        _ => fs::write(path, body)?,
    }
    // Cleanup must reject snapshots changed after discovery, even if submission succeeds.
    if mode == "alter_snapshot" {
        let snapshot = Path::new(field(&control, "editor_snapshot")?);
        ensure!(
            snapshot.starts_with(environment_path("ZPR_TEST_STATE")?),
            "Editor snapshot must be inside the fixture state"
        );
        fs::set_permissions(snapshot, fs::Permissions::from_mode(0o600))?;
        fs::write(snapshot, "Changed while the review editor was open\n")?;
    }
    if matches!(mode.as_str(), "change_head" | "change_base") {
        let (field, other) = if mode == "change_head" {
            ("head", "base")
        } else {
            ("base", "head")
        };
        control[field] = control[other].clone();
    }
    if mode == "accept_delayed_review" {
        let object = control
            .as_object_mut()
            .context("Expected editor controls")?;
        let review = object
            .remove("delayed_review")
            .context("No delayed review")?;
        let comments = object
            .remove("delayed_comments")
            .context("No delayed review comments")?;
        let id = review["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| review["id"].to_string());
        object
            .entry("reviews")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Reviews must be an array")?
            .push(review);
        object
            .entry("review_comments")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("Review comments must be an object")?
            .insert(id, comments);
    }
    if matches!(
        mode.as_str(),
        "change_head" | "change_base" | "accept_delayed_review"
    ) {
        let temporary = control_path.with_extension("next");
        fs::write(&temporary, serde_json::to_vec(&control)?)?;
        fs::rename(temporary, control_path)?;
    }
    Ok(if mode == "fail" { 12 } else { 0 })
}

fn zed(args: Vec<OsString>) -> Result<i32> {
    let state = environment_path("ZPR_TEST_STATE")?;
    let active = state.join("active.json");
    let mut checkouts = BTreeMap::new();
    collect_checkouts(&state, &state, &mut checkouts)?;
    append_json(
        &environment_path("ZPR_TEST_ZED_LOG")?,
        &json!({
            "args": argument_strings(&args)?,
            "cwd": std::env::current_dir()?,
            "active": if active.exists() { read_json(&active)? } else { Value::Null },
            "checkouts": checkouts,
        }),
    )?;
    let control = read_json(&environment_path("ZPR_TEST_CONTROL")?)?;
    Ok(control["zed_exit"].as_i64().unwrap_or(0) as i32)
}

fn collect_checkouts(
    directory: &Path,
    state: &Path,
    checkouts: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == "last-checkout.json" {
            checkouts.insert(
                path.strip_prefix(state)?.to_string_lossy().into_owned(),
                read_json(&path)?,
            );
        }
        // Match Path.rglob without following symlinked directories.
        if entry.file_type()?.is_dir() {
            collect_checkouts(&path, state, checkouts)?;
        }
    }
    Ok(())
}

fn gate(config: &Value, args: Vec<OsString>) -> Result<i32> {
    if let Some(target) = config["target"].as_str() {
        let pause = Path::new(field(config, "pause")?);
        let endpoint = field(config, "endpoint")?;
        if args.iter().any(|argument| argument == endpoint) && pause.exists() {
            fs::write(field(config, "started")?, "reading\n")?;
            let deadline = Instant::now()
                + Duration::from_millis(config["timeout_ms"].as_u64().unwrap_or(15_000));
            while pause.exists() {
                if Instant::now() >= deadline {
                    eprintln!("Timed out waiting for the terminal test to release the read");
                    return Ok(98);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        return Err(Command::new(target).args(args).exec())
            .context("Cannot continue the gated fixture command");
    }
    if args
        .iter()
        .any(|argument| argument == "repos/acme/widgets/pulls/18/files?per_page=100")
    {
        let root = Path::new(field(config, "root")?);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("prefetch-entered"))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !root.join("prefetch-release").exists() {
            ensure!(
                Instant::now() < deadline,
                "Timed out waiting for the prefetch test gate"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    super::gh::run(&json!({"tool": "gh"}), args)
}
