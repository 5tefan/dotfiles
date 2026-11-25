//! Local transport wrappers for checkout tests. Real tools come from `real_git` /
//! `real_jj` or ZPR_REAL_GIT / ZPR_REAL_JJ, never from the wrapper's PATH.
//! `transport-jj` accepts an `after_success` object with commands, path, and contents;
//! `transport-hook` writes "unsafe" to its configured marker path.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const BASE_URL: &str = "https://github.com/upstream/project.git";
const FORK_URL: &str = "https://github.com/contributor/fork.git";

pub fn run(config: &Value, args: Vec<OsString>) -> Result<i32> {
    match string(config, "tool")? {
        "transport-git" => git(config, args),
        "transport-jj" => jj(config, args),
        "transport-hook" => {
            fs::write(string(config, "marker")?, "unsafe")?;
            Ok(0)
        }
        tool => bail!("Unknown transport helper: {tool}"),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing string field {key}: {value}"))
}

fn fixture() -> Result<PathBuf> {
    Ok(std::env::var_os("ZPR_FIXTURE")
        .context("Missing ZPR_FIXTURE")?
        .into())
}

fn real_tool(config: &Value, key: &str, variable: &str) -> Result<PathBuf> {
    let path = match config.get(key) {
        Some(value) => PathBuf::from(value.as_str().with_context(|| format!("Invalid {key}"))?),
        None => PathBuf::from(
            std::env::var_os(variable).with_context(|| format!("Missing {variable}"))?,
        ),
    };
    ensure!(path.is_absolute(), "Real tool must be absolute: {path:?}");
    Ok(path)
}

fn append(path: &Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

fn log_args(path: &Path, args: &[OsString]) -> Result<()> {
    let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
    let mut line = serde_json::to_vec(&args)?;
    line.push(b'\n');
    append(path, &line)
}

fn has(args: &[OsString], expected: &str) -> bool {
    args.iter().any(|arg| arg == expected)
}

fn status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn git(config: &Value, mut args: Vec<OsString>) -> Result<i32> {
    let root = fixture()?;
    let real_git = real_tool(config, "real_git", "ZPR_REAL_GIT")?;
    log_args(&root.join("git-calls.jsonl"), &args)?;
    ensure!(
        !["push", "pull", "clone"]
            .iter()
            .any(|command| has(&args, command)),
        "Refusing external Git transport: {args:?}"
    );
    if has(&args, "ls-remote") {
        ensure!(
            has(&args, "--get-url"),
            "Refusing network ls-remote: {args:?}"
        );
    }
    if has(&args, "fetch") {
        let (index, local) = validate_fetch(&root, &args)?;
        args[index] = local.into_os_string();
        // Only this child may use file transport, after the HTTPS request was validated.
        let status = Command::new(&real_git)
            .args(&args)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .status()?;
        inject_pending(&root, &real_git, "first")?;
        if std::env::var("ZPR_SCENARIO").as_deref() == Ok("api-race") {
            let path = root.join("control.json");
            let mut control: Value = serde_json::from_slice(&fs::read(&path)?)?;
            control["base"]["ref"] = Value::String("moved".into());
            fs::write(path, serde_json::to_vec(&control)?)?;
        }
        return Ok(status_code(status));
    }
    if has(&args, "update-ref") && has(&args, "refs/remotes/origin/feature/topic") {
        let status = Command::new(&real_git).args(&args).status()?;
        inject_pending(&root, &real_git, "second")?;
        return Ok(status_code(status));
    }
    Err(Command::new(&real_git).args(&args).exec()).with_context(|| format!("Exec {real_git:?}"))
}

fn validate_fetch(root: &Path, args: &[OsString]) -> Result<(usize, PathBuf)> {
    let args: Vec<_> = args
        .iter()
        .map(|arg| arg.to_str().context("Non-UTF-8 fetch argument"))
        .collect::<Result<_>>()?;
    let requested: Vec<_> = args
        .iter()
        .enumerate()
        .filter(|(_, arg)| matches!(**arg, BASE_URL | FORK_URL))
        .collect();
    ensure!(
        requested.len() == 1,
        "Expected one allowed fetch URL: {args:?}"
    );
    let (url_index, expected) = requested[0];
    let separator = args
        .iter()
        .position(|arg| *arg == "--")
        .context("Fetch must separate the URL and refspecs with --")?;
    ensure!(
        url_index == separator + 1,
        "Unexpected fetch repository: {args:?}"
    );
    let refspecs = &args[separator + 2..];
    let destination = refspecs
        .first()
        .and_then(|spec| spec.split_once(':'))
        .map(|(_, destination)| destination)
        .context("Missing fetch destination")?;
    let (namespace, _) = destination
        .rsplit_once('/')
        .context("Missing fetch namespace")?;
    let prefetch = namespace.starts_with("refs/zed-pr-review-prefetch/");
    if prefetch {
        let hash = namespace
            .strip_prefix("refs/zed-pr-review-prefetch/")
            .unwrap();
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "Invalid prefetch namespace: {args:?}"
        );
    } else {
        ensure!(
            namespace.starts_with("refs/heads/zed-pr-review/"),
            "Invalid checkout namespace: {args:?}"
        );
    }
    let control: Value = serde_json::from_slice(&fs::read(root.join("control.json"))?)?;
    let branch = string(&control["head"], "ref")?;
    let expected_refs = if refspecs.iter().any(|spec| spec.starts_with("refs/pull/")) {
        ensure!(
            *expected == BASE_URL,
            "PR refs must come from the base repository: {args:?}"
        );
        let base = string(&control["base"], "ref")?;
        let mut refs = vec![
            format!("refs/pull/42/head:{namespace}/head"),
            format!("refs/heads/{base}:{namespace}/base"),
        ];
        if prefetch && refspecs.len() == 3 {
            ensure!(
                string(&control["head"]["repo"], "full_name")? == "upstream/project",
                "A combined fetch requires a same-repository source: {args:?}"
            );
            refs.push(format!("refs/heads/{branch}:{namespace}/source"));
        }
        refs
    } else {
        vec![format!("refs/heads/{branch}:{namespace}/source")]
    };
    ensure!(
        refspecs
            .iter()
            .copied()
            .eq(expected_refs.iter().map(String::as_str)),
        "Unexpected fetch refspecs: {args:?}; expected {expected_refs:?}"
    );
    ensure!(
        !args.iter().any(|arg| arg.starts_with('+')),
        "Forced fetch: {args:?}"
    );
    ensure!(
        std::env::var("GIT_ALLOW_PROTOCOL").as_deref() == Ok("https"),
        "Fetch must restrict GIT_ALLOW_PROTOCOL to https"
    );
    for flag in [
        "--atomic",
        "--no-tags",
        "--no-prune",
        "--no-prune-tags",
        "--refmap=",
        "--no-write-fetch-head",
        "--no-recurse-submodules",
        "--no-auto-maintenance",
        "--no-write-commit-graph",
        "--no-filter",
        "http.followRedirects=false",
        "core.hooksPath=/dev/null",
    ] {
        ensure!(args.contains(&flag), "Missing {flag}: {args:?}");
    }
    Ok((
        url_index,
        root.join(if *expected == BASE_URL {
            "base.git"
        } else {
            "fork.git"
        }),
    ))
}

fn inject_pending(root: &Path, real_git: &Path, stage: &str) -> Result<()> {
    let path = root.join("pending-ref.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let change: Value = serde_json::from_slice(&bytes)?;
    if string(&change, "stage")? != stage {
        return Ok(());
    }
    let mut command = Command::new(real_git);
    command.args([
        "--no-pager",
        "--git-dir",
        string(&change, "git_dir")?,
        "-c",
        "core.hooksPath=/dev/null",
        "update-ref",
    ]);
    let reference = string(&change, "reference")?;
    match change["target"]
        .as_str()
        .filter(|target| !target.is_empty())
    {
        Some(target) => command.args([reference, target]),
        None => command.args(["-d", reference]),
    };
    let status = command.status()?;
    ensure!(
        status.success(),
        "Pending ref injection at {stage} failed: {status}"
    );
    fs::rename(path, root.join("injected-ref.json"))?;
    Ok(())
}

fn jj(config: &Value, args: Vec<OsString>) -> Result<i32> {
    let root = fixture()?;
    let real_jj = real_tool(config, "real_jj", "ZPR_REAL_JJ")?;
    log_args(&root.join("jj-calls.jsonl"), &args)?;
    let result = Command::new(real_jj)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .output()?;
    io::stderr().write_all(&result.stderr)?;
    if !result.status.success() {
        append(&root.join("jj-errors.log"), &result.stderr)?;
    } else if let Some(after) = config.get("after_success") {
        let commands = after["commands"]
            .as_array()
            .context("Missing after_success commands")?;
        let mut matches = false;
        for command in commands {
            matches |= has(
                &args,
                command.as_str().context("Invalid after_success command")?,
            );
        }
        if matches {
            fs::write(string(after, "path")?, string(after, "contents")?)?;
        }
    }
    Ok(status_code(result.status))
}
