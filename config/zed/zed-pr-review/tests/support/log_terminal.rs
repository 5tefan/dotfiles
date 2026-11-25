//! Real jj checkout and terminal navigation regressions.
#![cfg(unix)]

use super::pty::{self, Terminal};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

struct Repository {
    path: PathBuf,
    env: BTreeMap<OsString, OsString>,
    changes: BTreeMap<String, String>,
}

impl Repository {
    fn new(root: &Path) -> Result<Self> {
        let path = root.join("checkout with spaces");
        let home = root.join("home");
        let tools = root.join("bin");
        let temporary = root.join("tmp");
        for path in [&path, &home, &tools, &temporary] {
            fs::create_dir(path)?;
        }
        for program in ["gh", "viddy"] {
            crate::install(
                &tools.join(program),
                &json!({
                    "tool": "deny", "message": format!("Unexpected {program} invocation")
                }),
            )?;
        }
        let config = root.join("jj.toml");
        fs::write(
            &config,
            r#"[user]
name = "Terminal Tests"
email = "terminal@example.test"
[ui]
pager = "cat"
[aliases]
l = ["log", "-r", "all()"]
[templates]
log = 'format_short_change_id_with_change_offset(self) ++ " " ++ description.first_line() ++ "\n"'
[template-aliases]
"format_short_change_id_with_change_offset(commit)" = "commit.change_id().short(12)"
"#,
        )?;
        let inherited_path = std::env::var_os("PATH").context("Missing PATH")?;
        let search = std::env::join_paths(
            std::iter::once(tools).chain(std::env::split_paths(&inherited_path)),
        )?;
        // Construct, rather than filter, the environment: no user Git/JJ
        // configuration, credentials, Zed context, state override, or transport.
        let mut env: BTreeMap<OsString, OsString> = [
            ("PATH", search),
            ("HOME", home.clone().into_os_string()),
            ("XDG_CONFIG_HOME", home.into_os_string()),
            ("TMPDIR", temporary.into_os_string()),
            ("JJ_CONFIG", config.clone().into_os_string()),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
        for (key, value) in [
            ("JJ_PAGER", "cat"),
            ("PAGER", "cat"),
            ("TERM", "xterm-256color"),
            ("LANG", "C"),
            ("LC_ALL", "C"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_ALLOW_PROTOCOL", ""),
        ] {
            env.insert(key.into(), value.into());
        }
        let mut repo = Self {
            path,
            env,
            changes: BTreeMap::new(),
        };
        repo.jj(&["git", "init"])?;
        for index in 0..18 {
            if index != 0 {
                repo.jj(&["new"])?;
            }
            fs::write(
                repo.path.join("file.txt"),
                format!("contents of revision {index}\n"),
            )?;
            repo.jj(&["describe", "-m", &format!("revision-{index:02}")])?;
        }
        let records = repo.jj(&[
            "log",
            "--no-graph",
            "-r",
            "all() ~ root()",
            "-T",
            r#"json({"name" => description.first_line(), "id" => change_id}) ++ "\n""#,
        ])?;
        for line in records.lines() {
            let entry: Value = serde_json::from_str(line)?;
            repo.changes.insert(
                entry["name"].as_str().context("Revision name")?.into(),
                entry["id"].as_str().context("Revision change ID")?.into(),
            );
        }
        writeln!(
            OpenOptions::new().append(true).open(config)?,
            "\n[revset-aliases]\n\"immutable_heads()\" = {}",
            serde_json::to_string(&repo.changes["revision-00"])?
        )?;
        ensure!(
            repo.jj(&[
                "log",
                "--no-graph",
                "-r",
                &repo.changes["revision-00"],
                "-T",
                "immutable"
            ])? == "true",
            "Fixture requires an immutable revision"
        );
        repo.jj(&["edit", &repo.changes["revision-03"]])?;
        Ok(repo)
    }

    fn command(&self, binary: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(binary);
        command.current_dir(&self.path).env_clear().envs(&self.env);
        command
    }

    fn jj(&self, args: &[&str]) -> Result<String> {
        let result = pty::output(
            self.command("jj")
                .args(["--no-pager", "--color=never"])
                .args(args),
            Duration::from_secs(10),
        )?;
        ensure!(
            result.status.success(),
            "jj {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(String::from_utf8(result.stdout)?.trim().to_owned())
    }

    fn current(&self) -> Result<String> {
        self.jj(&[
            "--ignore-working-copy",
            "--at-operation=@",
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "change_id",
        ])
    }
}

fn navigation(terminal: &mut Terminal, repo: &Repository) -> Result<()> {
    terminal.send(b"g")?;
    terminal.wait(|t| Ok(t.screen.view()?.0 == 1), "scroll to top")?;
    ensure!(
        !terminal
            .screen
            .current_visible(&repo.changes["revision-03"]),
        "@ remained visible at the top"
    );
    let viewport = terminal.screen.viewport();
    // A changed visible description proves that a periodic refresh completed;
    // sleeping alone could pass even if the refresh loop stopped entirely.
    repo.jj(&[
        "describe",
        "-r",
        &repo.changes["revision-17"],
        "-m",
        "revision-17-refreshed",
    ])?;
    terminal.wait(
        |t| {
            Ok(t.screen
                .body()
                .iter()
                .any(|line| line.contains("revision-17-refreshed")))
        },
        "periodic refresh of the visible description",
    )?;
    terminal.keep(
        |t| {
            Ok(t.screen.viewport() == viewport
                && !t.screen.current_visible(&repo.changes["revision-03"]))
        },
        Duration::from_millis(2200),
        "Periodic refresh moved the viewport back to @",
    )?;

    let moved = &repo.changes["revision-01"];
    repo.jj(&["edit", moved])?;
    terminal.wait(
        |t| Ok(t.screen.current_visible(moved)),
        "external checkout to become visible",
    )?;
    let target = &repo.changes["revision-02"];
    let point = terminal.screen.point(&target[..12])?;
    terminal.mouse(0, point, false)?;
    terminal.keep(
        |t| Ok(t.screen.current_visible(moved)),
        Duration::from_millis(200),
        "Mouse down edited a revision",
    )?;
    ensure!(
        repo.current()? == *moved,
        "A mouse press alone must not edit"
    );
    terminal.mouse(32, (point.0 + 1, point.1), false)?;
    terminal.mouse(0, point, true)?;
    terminal.keep(
        |t| Ok(t.screen.current_visible(moved)),
        Duration::from_millis(200),
        "A drag edited a revision",
    )?;
    ensure!(repo.current()? == *moved, "Dragging must not edit");
    terminal.click(point)?;
    terminal.wait(
        |t| {
            let lines = t.screen.lines();
            Ok(t.screen.current_visible(target)
                && lines[lines.len() - 2].contains(&format!("Editing {target}")))
        },
        "click to edit the mutable revision",
    )?;
    ensure!(
        repo.current()? == *target,
        "Click did not perform a real jj edit"
    );
    ensure!(
        fs::read_to_string(repo.path.join("file.txt"))? == "contents of revision 2\n",
        "Click did not update the checkout contents"
    );

    terminal.send(b"G")?;
    terminal.wait(
        |t| {
            let (_, last, count) = t.screen.view()?;
            Ok(last == count)
        },
        "scroll to bottom",
    )?;
    terminal.click(terminal.screen.point(&repo.changes["revision-00"][..12])?)?;
    terminal.wait(
        |t| {
            let lines = t.screen.lines();
            Ok(lines[lines.len() - 2].contains("Immutable revision; no checkout was attempted."))
        },
        "immutable click refusal",
    )?;
    ensure!(
        repo.current()? == *target,
        "Immutable click changed the checkout"
    );
    terminal.exit_and_assert_restored(b"q")
}

pub fn run(args: Vec<OsString>) -> Result<i32> {
    ensure!(
        args.len() == 3,
        "log-terminal expects binary, fixture root, scenario"
    );
    let scenario = args[2].to_str().context("Invalid scenario")?;
    ensure!(
        matches!(scenario, "navigation" | "ctrl-c"),
        "Unknown log scenario {scenario}"
    );
    pty::session(Duration::from_secs(60))?;
    let repo = Repository::new(Path::new(&args[1]))?;
    let mut command = repo.command(&args[0]);
    command.args(["stack", "--watch"]);
    let mut terminal = Terminal::open(&mut command, 10, 80, true)?;
    let result = (|| -> Result<()> {
        terminal.wait(
            |t| Ok(t.screen.current_visible(&repo.changes["revision-03"])),
            "initial @ below the first page",
        )?;
        ensure!(
            terminal.screen.view()?.0 > 1,
            "Fixture did not put @ below the first page"
        );
        terminal.assert_active()?;
        match scenario {
            "navigation" => navigation(&mut terminal, &repo),
            _ => terminal.exit_and_assert_restored(b"\x03"),
        }
    })();
    result.with_context(|| terminal.transcript())?;
    drop(terminal);
    ensure!(
        !repo.path.join("scratch/zed-pr-review").exists(),
        "Viewer created review state"
    );
    println!("PTY scenario {scenario} passed");
    Ok(0)
}
