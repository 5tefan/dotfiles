#![cfg(unix)]
//! Local-only transport tests. The child process has isolated HOME/JJ_CONFIG/PATH.
//! Its Git shim substitutes local bare repositories for the two allowed HTTPS URLs;
//! the real Git still performs every fetch, ref update, and object import.

mod support;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use zed_pr_review::github::{Client, workspace_root};

const URL: &str = "https://github.com/upstream/project.git";

fn output(command: &mut Command, input: &[u8]) -> Result<String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input)?;
    let result = child.wait_with_output()?;
    ensure!(
        result.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    server: PathBuf,
    real_git: PathBuf,
    real_jj: PathBuf,
    path: std::ffi::OsString,
    common: String,
    head: String,
    base: String,
}

impl Fixture {
    fn new(colocate: bool) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("zpr-transport-")
            .tempdir()?;
        let root = temp.path().canonicalize()?;
        let path = std::env::var_os("PATH").context("Missing PATH")?;
        let real_git = std::env::split_paths(&path)
            .map(|p| p.join("git"))
            .find(|p| p.is_file())
            .context("Git is required")?
            .canonicalize()?;
        let real_jj = std::env::split_paths(&path)
            .map(|p| p.join("jj"))
            .find(|p| p.is_file())
            .context("jj is required")?
            .canonicalize()?;
        let repo = root.join("workspace with spaces");
        let server = root.join("base.git");
        for dir in [&repo, &server, &root.join("home"), &root.join("bin")] {
            fs::create_dir_all(dir)?;
        }
        fs::write(
            root.join("jj.toml"),
            "[user]\nname = 'Transport Test'\nemail = 'transport@example.test'\n[ui]\npager = 'cat'\n",
        )?;
        let mut f = Self {
            _temp: temp,
            root,
            repo,
            server,
            real_git,
            real_jj,
            path,
            common: String::new(),
            head: String::new(),
            base: String::new(),
        };
        output(
            f.command(&f.real_git)
                .args(["--no-pager", "init", "--bare"])
                .arg(&f.server),
            b"",
        )?;
        f.common = f.commit("common\n", None)?;
        f.head = f.commit("fork head\n", Some(&f.common))?;
        f.base = f.commit("base diverged\n", Some(&f.common))?;
        output(
            f.command(&f.real_git)
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&f.server)
                .args(["update-ref", "--stdin"]),
            format!(
                "update refs/heads/main {base}\nupdate refs/heads/feature/topic {base}\nupdate refs/pull/42/head {head}\n",
                base = f.base,
                head = f.head,
            )
            .as_bytes(),
        )?;
        f.jj(&[
            "git",
            "init",
            if colocate {
                "--colocate"
            } else {
                "--no-colocate"
            },
        ])?;
        f.jj(&["describe", "-m", "Existing local work"])?;
        fs::write(
            f.root.join("control.json"),
            serde_json::to_vec(&json!({
                "number":42, "state":"open", "merged":false, "merged_at":null,
                "html_url":"https://github.com/upstream/project/pull/42", "title":"Fork PR",
                "user":{"login":"author"},
                "head":{"sha":f.head,"ref":"feature/topic","repo":{"full_name":"contributor/fork"}},
                "base":{"sha":f.base,"ref":"main","repo":{"full_name":"upstream/project"}}
            }))?,
        )?;
        for (name, config) in [
            (
                "git",
                json!({"tool": "transport-git", "real_git": f.real_git}),
            ),
            ("gh", json!({"tool": "transport-gh"})),
            ("jj", json!({"tool": "transport-jj", "real_jj": f.real_jj})),
        ] {
            support::install(&f.root.join("bin").join(name), &config)?;
        }
        let mut paths = vec![f.root.join("bin")];
        paths.extend(std::env::split_paths(&f.path));
        f.path = std::env::join_paths(paths)?;
        Ok(f)
    }

    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(executable);
        c.env_clear()
            .current_dir(&self.repo)
            .env("PATH", &self.path)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home"))
            .env("JJ_CONFIG", self.root.join("jj.toml"))
            .env("JJ_PAGER", "cat")
            // Tiny isolated repositories do not benefit from a full Rayon pool per jj process.
            .env("RAYON_NUM_THREADS", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .env("ZPR_FIXTURE", &self.root)
            .env("ZPR_REAL_GIT", &self.real_git)
            .env("ZPR_REAL_JJ", &self.real_jj);
        c
    }
    fn jj(&self, args: &[&str]) -> Result<String> {
        output(self.command("jj").arg("--no-pager").args(args), b"")
    }
    fn git(&self, args: &[&str]) -> Result<String> {
        output(
            self.command(&self.real_git)
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&self.server)
                .args(args),
            b"",
        )
    }
    fn commit(&self, contents: &str, parent: Option<&str>) -> Result<String> {
        let mut c = self.command(&self.real_git);
        c.arg("--no-pager").arg("--git-dir").arg(&self.server);
        let blob = output(
            c.args(["hash-object", "-w", "--stdin"]),
            contents.as_bytes(),
        )?;
        let mut c = self.command(&self.real_git);
        c.arg("--no-pager").arg("--git-dir").arg(&self.server);
        let tree = output(
            c.arg("mktree"),
            format!("100644 blob {blob}\tfile.txt\n").as_bytes(),
        )?;
        let mut c = self.command(&self.real_git);
        c.arg("--no-pager")
            .arg("--git-dir")
            .arg(&self.server)
            .args(["commit-tree", &tree]);
        if let Some(parent) = parent {
            c.args(["-p", parent]);
        }
        output(&mut c, contents.as_bytes())
    }
    fn run(&self, scenario: &str) -> Result<()> {
        output(
            self.command(std::env::current_exe()?)
                .args(["--exact", "checkout_child", "--nocapture"])
                .env("ZPR_SCENARIO", scenario)
                .env("ZPR_COMMON", &self.common),
            b"",
        )
        .with_context(|| fs::read_to_string(self.root.join("jj-errors.log")).unwrap_or_default())?;
        Ok(())
    }
}

// Preserve the former group names as filters while scheduling each isolated case separately.
macro_rules! transport_scenarios {
    ($group:ident, $runner:ident, { $($name:ident: ($($args:expr),+)),+ $(,)? }) => {
        mod $group {
            mod noncolocated {
                $(
                    #[test]
                    fn $name() -> anyhow::Result<()> {
                        crate::$runner(false, $($args),+)
                    }
                )+
            }

            mod colocated {
                $(
                    #[test]
                    fn $name() -> anyhow::Result<()> {
                        crate::$runner(true, $($args),+)
                    }
                )+
            }
        }
    };
}

fn run_scenario(colocate: bool, scenario: &str) -> Result<()> {
    Fixture::new(colocate)?
        .run(scenario)
        .with_context(|| format!("colocate={colocate}, scenario={scenario}"))
}

transport_scenarios!(local_transport_colocated_and_noncolocated, run_scenario, {
    dirty: ("dirty"),
    empty: ("empty"),
    obstruction: ("obstruction"),
    directory_obstruction: ("directory-obstruction"),
    target_conflicts: ("target-conflicts"),
    sparse: ("sparse"),
    shallow: ("shallow"),
    conflicts: ("conflicts"),
    stale: ("stale"),
    api_race: ("api-race"),
    ref_race: ("ref-race"),
    url_rewrite: ("url-rewrite"),
});

transport_scenarios!(prefetch_reuses_transport_without_workspace_changes, run_scenario, {
    prefetch_no_source: ("prefetch-no-source"),
    prefetch_same_repo: ("prefetch-same-repo"),
    prefetch_fork: ("prefetch-fork"),
    prefetch_base_ahead: ("prefetch-base-ahead"),
    prefetch_shallow: ("prefetch-shallow"),
});

transport_scenarios!(prefetch_stale_pins_and_source_configuration_miss, run_scenario, {
    prefetch_stale_head: ("prefetch-stale-head"),
    prefetch_stale_base: ("prefetch-stale-base"),
    prefetch_source_added: ("prefetch-source-added"),
    prefetch_source_renamed: ("prefetch-source-renamed"),
    prefetch_source_removed: ("prefetch-source-removed"),
});

transport_scenarios!(prefetch_validates_refs_and_preserves_foreground_guards, run_scenario, {
    prefetch_corrupt_base: ("prefetch-corrupt-base"),
    prefetch_corrupt_source: ("prefetch-corrupt-source"),
    prefetch_symbolic: ("prefetch-symbolic"),
    prefetch_pending: ("prefetch-pending"),
    prefetch_source_conflict: ("prefetch-source-conflict"),
    prefetch_url_rewrite: ("prefetch-url-rewrite"),
});

fn prefetch_checkout(
    client: &Client,
    mut pr: zed_pr_review::model::PullRequest,
    fixture: &Path,
    scenario: &str,
) -> Result<()> {
    let directory = &client.root;
    let git_calls = fixture.join("git-calls.jsonl");
    let calls = || -> Result<Vec<Vec<String>>> {
        fs::read_to_string(&git_calls)?
            .lines()
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect()
    };
    let fetch_count = || -> Result<usize> {
        Ok(calls()?
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "fetch"))
            .count())
    };
    let real_git = std::env::var_os("ZPR_REAL_GIT").unwrap();
    let server = fixture.join("base.git");
    let server_git = |store: &Path, args: &[&str]| -> Result<String> {
        output(
            Command::new(&real_git)
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(store)
                .args(args),
            b"",
        )
    };
    let with_source = matches!(
        scenario,
        "prefetch-same-repo"
            | "prefetch-fork"
            | "prefetch-source-renamed"
            | "prefetch-source-removed"
            | "prefetch-corrupt-source"
            | "prefetch-source-conflict"
    );
    let mut control: Value = serde_json::from_slice(&fs::read(fixture.join("control.json"))?)?;
    if scenario == "prefetch-source-added" {
        control["head"]["repo"]["full_name"] = json!("upstream/project");
    }
    if with_source {
        let store = if scenario == "prefetch-fork" {
            let fork = fixture.join("fork.git");
            output(
                Command::new(&real_git)
                    .args(["--no-pager", "clone", "--bare"])
                    .arg(&server)
                    .arg(&fork),
                b"",
            )?;
            git(
                client,
                &[
                    "remote",
                    "add",
                    "source",
                    "git@github.com:contributor/fork.git",
                ],
            )?;
            fork
        } else {
            control["head"]["repo"]["full_name"] = json!("upstream/project");
            git(client, &["remote", "add", "source", URL])?;
            server.clone()
        };
        server_git(
            &store,
            &["update-ref", "refs/heads/feature/topic", &pr.head_sha],
        )?;
    }
    fs::write(fixture.join("control.json"), serde_json::to_vec(&control)?)?;
    pr = client.read_pr(42)?;
    let mut base_tip = pr.base_sha.clone();
    if scenario == "prefetch-base-ahead" {
        let tree = server_git(
            &server,
            &["rev-parse", &format!("{}^{{tree}}", pr.base_sha)],
        )?;
        base_tip = server_git(
            &server,
            &[
                "commit-tree",
                &tree,
                "-p",
                &pr.base_sha,
                "-m",
                "Base advanced",
            ],
        )?;
        server_git(&server, &["update-ref", "refs/heads/main", &base_tip])?;
    }
    if scenario == "prefetch-shallow" {
        server_git(
            &client.git_dir,
            &[
                "fetch",
                "--depth=1",
                "--no-write-fetch-head",
                &format!("file://{}", server.display()),
                "refs/pull/42/head:refs/zed-pr-review-shallow-fixture",
            ],
        )?;
    }
    let kept_commit = client.checkout_commit()?;
    for reference in [
        "refs/heads/user-kept",
        "refs/remotes/unrelated/kept",
        "refs/tags/user-kept",
    ] {
        git(client, &["update-ref", reference, &kept_commit])?;
    }
    jj(directory, &["--ignore-working-copy", "git", "import"])?;
    jj(directory, &["bookmark", "create", "user-local", "-r", "@"])?;
    let recorded = client.checkout_commit()?;
    let visible_refs = || {
        git(
            client,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/",
                "refs/remotes/",
                "refs/tags/",
            ],
        )
    };
    let bookmarks = || {
        jj(
            directory,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "--all-remotes",
                "-T",
                r#"name ++ "\t" ++ remote ++ "\t" ++ conflict ++ "\t" ++ if(normal_target, normal_target.commit_id()) ++ "\t" ++ tracked ++ "\n""#,
            ],
        )
    };
    let refs_before = visible_refs()?;
    let bookmarks_before = bookmarks()?;
    let hooks = fixture.join("hooks");
    fs::create_dir(&hooks)?;
    let hook = hooks.join("reference-transaction");
    support::install(
        &hook,
        &json!({"tool": "transport-hook", "marker": fixture.join("hook-ran")}),
    )?;
    git(
        client,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    )?;
    fs::write(directory.join("keep.txt"), "unsnapshotted local changes\n")?;
    fs::write(client.git_dir.join("FETCH_HEAD"), "keep fetch head\n")?;
    if scenario == "prefetch-url-rewrite" {
        git(
            client,
            &[
                "config",
                &format!("url.file://{}.insteadOf", server.display()),
                URL,
            ],
        )?;
    }
    let jj_before = fs::read(fixture.join("jj-calls.jsonl"))?;
    let gh_before = fs::read(fixture.join("gh-calls.jsonl"))?;
    let prefetched = client.prefetch_pr(&pr, "main");
    assert_eq!(
        fs::read(fixture.join("jj-calls.jsonl"))?,
        jj_before,
        "background must not invoke jj at all"
    );
    assert_eq!(
        fs::read(fixture.join("gh-calls.jsonl"))?,
        gh_before,
        "background uses the caller's live PR pins"
    );
    assert_eq!(client.checkout_commit()?, recorded);
    assert_eq!(visible_refs()?, refs_before);
    assert_eq!(bookmarks()?, bookmarks_before);
    assert_eq!(
        fs::read_to_string(directory.join("keep.txt"))?,
        "unsnapshotted local changes\n"
    );
    assert!(!directory.join("file.txt").exists());
    assert_eq!(
        fs::read_to_string(client.git_dir.join("FETCH_HEAD"))?,
        "keep fetch head\n"
    );
    assert!(!fixture.join("hook-ran").exists());
    if scenario == "prefetch-url-rewrite" {
        assert!(format!("{:#}", prefetched.unwrap_err()).contains("URL rewriting"));
        assert_eq!(fetch_count()?, 0);
        return Ok(());
    }
    prefetched?;
    let network_fetches = if scenario == "prefetch-fork" { 2 } else { 1 };
    assert_eq!(fetch_count()?, network_fetches);
    let listing = git(
        client,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/zed-pr-review-prefetch/",
        ],
    )?;
    assert_eq!(listing.lines().count(), if with_source { 3 } else { 2 });
    let head_ref = listing
        .lines()
        .find_map(|line| {
            let (name, _) = line.split_once(' ')?;
            name.ends_with("/head").then_some(name.to_owned())
        })
        .unwrap();
    let namespace = head_ref.strip_suffix("/head").unwrap();
    assert!(listing.contains(&format!("{namespace}/base {base_tip}")));
    assert_eq!(
        git(client, &["rev-parse", "--is-shallow-repository"])?,
        "false"
    );

    // A title edit must reuse the same deterministic key. A warm prefetch inspects
    // all of its refs in one call, without scanning retained entries individually.
    pr.title = "Cosmetic title edit".into();
    let warm_start = calls()?.len();
    client.prefetch_pr(&pr, "main")?;
    let warm_calls = calls()?;
    let inspections: Vec<_> = warm_calls[warm_start..]
        .iter()
        .filter(|args| args.iter().any(|arg| arg == "for-each-ref"))
        .collect();
    assert_eq!(inspections.len(), 1);
    assert_eq!(inspections[0].last().unwrap(), &format!("{namespace}/"));
    assert_eq!(fetch_count()?, network_fetches);
    assert_eq!(fs::read(fixture.join("gh-calls.jsonl"))?, gh_before);

    let mut misses = true;
    let mut source_name = "source";
    match scenario {
        "prefetch-stale-head" => {
            control["head"]["sha"] = json!(pr.base_sha);
            server_git(&server, &["update-ref", "refs/pull/42/head", &pr.base_sha])?;
        }
        "prefetch-stale-base" => control["base"]["sha"] = json!(std::env::var("ZPR_COMMON")?),
        "prefetch-source-added" => {
            server_git(
                &server,
                &["update-ref", "refs/heads/feature/topic", &pr.head_sha],
            )?;
            git(client, &["remote", "add", "source", URL])?;
        }
        "prefetch-source-renamed" => {
            git(
                client,
                &[
                    "config",
                    "--rename-section",
                    "remote.source",
                    "remote.renamed",
                ],
            )?;
            source_name = "renamed";
        }
        "prefetch-source-removed" => {
            git(client, &["config", "--remove-section", "remote.source"])?;
        }
        "prefetch-corrupt-base" => {
            git(
                client,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "update-ref",
                    &format!("{namespace}/base"),
                    &pr.head_sha,
                ],
            )?;
        }
        "prefetch-corrupt-source" => {
            git(
                client,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "update-ref",
                    &format!("{namespace}/source"),
                    &pr.base_sha,
                ],
            )?;
        }
        "prefetch-symbolic" => {
            git(
                client,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "symbolic-ref",
                    &head_ref,
                    "refs/heads/user-kept",
                ],
            )?;
            let count = fetch_count()?;
            assert!(
                format!("{:#}", client.prefetch_pr(&pr, "main").unwrap_err()).contains("symbolic")
            );
            assert_eq!(fetch_count()?, count);
            assert_eq!(visible_refs()?, refs_before);
        }
        "prefetch-pending" => {
            git(
                client,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "update-ref",
                    "refs/tags/user-kept",
                    &pr.head_sha,
                ],
            )?;
            let error = format!("{:#}", client.fetch_pr(&pr).unwrap_err());
            assert!(error.contains("pending Git changes"), "{error}");
            assert_eq!(fetch_count()?, network_fetches);
            assert_eq!(client.checkout_commit()?, recorded);
            return Ok(());
        }
        "prefetch-source-conflict" => {
            jj(
                directory,
                &[
                    "--ignore-working-copy",
                    "bookmark",
                    "create",
                    "feature/topic",
                    "-r",
                    &recorded,
                ],
            )?;
            let error = format!("{:#}", client.fetch_pr(&pr).unwrap_err());
            assert!(error.contains("Local source bookmark"), "{error}");
            assert_eq!(fetch_count()?, network_fetches);
            assert_eq!(client.checkout_commit()?, recorded);
            return Ok(());
        }
        _ => misses = false,
    }
    fs::write(fixture.join("control.json"), serde_json::to_vec(&control)?)?;
    pr = client.read_pr(42)?;
    let before_foreground = fetch_count()?;
    client.fetch_pr(&pr)?;
    let tracks_source = (with_source && scenario != "prefetch-source-removed")
        || scenario == "prefetch-source-added";
    assert_eq!(
        fetch_count()?,
        before_foreground
            + if misses {
                if tracks_source { 2 } else { 1 }
            } else {
                0
            }
    );
    assert_eq!(client.checkout_commit()?, recorded);
    assert_eq!(
        git(client, &["rev-parse", "refs/heads/user-kept"])?,
        kept_commit
    );
    assert_eq!(
        git(client, &["rev-parse", "refs/tags/user-kept"])?,
        kept_commit
    );
    assert!(!fixture.join("hook-ran").exists());
    assert_eq!(
        fs::read_to_string(directory.join("keep.txt"))?,
        "unsnapshotted local changes\n"
    );
    assert_eq!(
        fs::read_to_string(client.git_dir.join("FETCH_HEAD"))?,
        "keep fetch head\n"
    );
    let private = git(
        client,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/zed-pr-review/",
        ],
    )?;
    assert!(
        private.contains(&format!("/head {}", pr.head_sha)),
        "{private}"
    );
    assert!(private.contains(&format!("/base {base_tip}")), "{private}");
    if tracks_source {
        assert!(
            private.contains(&format!("/source {}", pr.head_sha)),
            "{private}"
        );
        assert_eq!(
            git(
                client,
                &[
                    "rev-parse",
                    &format!("refs/remotes/{source_name}/feature/topic")
                ]
            )?,
            pr.head_sha
        );
        assert!(
            source_listing(client)?
                .contains(&format!("{source_name}\tfalse\t{}\ttrue", pr.head_sha))
        );
    }
    Ok(())
}

fn run_base_tip_scenario(colocate: bool, scenario: &str, shallow: bool) -> Result<()> {
    let f = Fixture::new(colocate)?;
    let actual = match scenario {
        "rewind" => f.common.clone(),
        "divergent" => f.commit("divergent base\n", Some(&f.common))?,
        _ => {
            let mut tip = f.base.clone();
            for n in 1..15 {
                tip = f.commit(&format!("newer base {n}\n"), Some(&tip))?;
            }
            // Using this tip as the review pin would make the merge base the
            // PR head and empty the review diff. Keep the original base pin.
            let tip = f.git(&[
                "commit-tree",
                &format!("{tip}^{{tree}}"),
                "-p",
                &tip,
                "-p",
                &f.head,
                "-m",
                "Merge PR head into the newer base",
            ])?;
            assert_eq!(
                f.git(&[
                    "rev-list",
                    "--first-parent",
                    "--count",
                    &format!("{}..{tip}", f.base),
                ])?,
                "15"
            );
            tip
        }
    };
    f.git(&["update-ref", "refs/heads/main", &actual])?;
    fs::write(f.root.join("actual-base"), &actual)?;
    if scenario == "unavailable" {
        // A real commit exists on the server, but no fetched ref reaches it.
        let missing = f.commit("unavailable pinned base\n", Some(&f.base))?;
        let mut control: Value = serde_json::from_slice(&fs::read(f.root.join("control.json"))?)?;
        control["base"]["sha"] = json!(missing);
        fs::write(f.root.join("control.json"), serde_json::to_vec(&control)?)?;
    }
    let child_scenario =
        if ["api-race", "ref-race"].contains(&scenario) || scenario.starts_with("pending-") {
            assert!(!shallow);
            scenario.to_owned()
        } else {
            format!(
                "base-tip-{scenario}{}",
                if shallow { "-shallow" } else { "" }
            )
        };
    f.run(&child_scenario)
        .with_context(|| format!("colocate={colocate}, shallow={shallow}, scenario={scenario}"))
}

transport_scenarios!(base_tip_fast_forward_colocated_and_noncolocated, run_base_tip_scenario, {
    fast_forward: ("fast-forward", false),
});

transport_scenarios!(base_tip_fast_forward_shallow_colocated_and_noncolocated, run_base_tip_scenario, {
    fast_forward: ("fast-forward", true),
});

transport_scenarios!(base_tip_refusals_colocated_and_noncolocated, run_base_tip_scenario, {
    rewind: ("rewind", false),
    divergent: ("divergent", false),
    unavailable: ("unavailable", false),
});

transport_scenarios!(base_tip_refusals_shallow_colocated_and_noncolocated, run_base_tip_scenario, {
    rewind: ("rewind", true),
    divergent: ("divergent", true),
    unavailable: ("unavailable", true),
});

transport_scenarios!(base_tip_pending_private_ref_colocated_and_noncolocated, run_base_tip_scenario, {
    unrelated_ref: ("unrelated-ref", false),
    unrelated_ref_shallow: ("unrelated-ref", true),
});

transport_scenarios!(base_tip_existing_guards_colocated_and_noncolocated, run_base_tip_scenario, {
    api_race: ("api-race", false),
    ref_race: ("ref-race", false),
    pending_before_fallback_update: ("pending-before-fallback-update", false),
    pending_first_fallback_update: ("pending-first-fallback-update", false),
});

transport_scenarios!(immutable_heads_colocated_and_noncolocated, run_scenario, {
    immutable: ("immutable"),
    immutable_late_edit: ("immutable-late-edit"),
    ignore_removed_immutable: ("ignore-removed-immutable"),
    ignore_kept_immutable: ("ignore-kept-immutable"),
    ignore_late_file_immutable: ("ignore-late-file-immutable"),
});

transport_scenarios!(ignored_files_colocated_and_noncolocated, run_scenario, {
    ignore_removed: ("ignore-removed"),
    ignore_kept: ("ignore-kept"),
    ignore_negated: ("ignore-negated"),
    ignore_node_modules: ("ignore-node-modules"),
    ignore_node_modules_contents: ("ignore-node-modules-contents"),
    ignore_nested_removed: ("ignore-nested-removed"),
    ignore_nested_kept: ("ignore-nested-kept"),
    ignore_excludes: ("ignore-excludes"),
    ignore_relative_excludes: ("ignore-relative-excludes"),
    ignore_core_changed: ("ignore-core-changed"),
    ignore_late_file: ("ignore-late-file"),
});

transport_scenarios!(ignored_sibling_directories_use_bounded_batches, run_scenario, {
    ignore_batched_directories: ("ignore-batched-directories"),
});

transport_scenarios!(ignored_sibling_leaves_use_bounded_batches, run_scenario, {
    ignore_batched_leaves: ("ignore-batched-leaves"),
});

transport_scenarios!(source_tracking_creation, run_scenario, {
    track_same_repo: ("track-same-repo"),
    track_fork_present: ("track-fork-present"),
    track_fork_absent: ("track-fork-absent"),
    track_wrong_host: ("track-wrong-host"),
    track_local_equal: ("track-local-equal"),
    track_literal_name: ("track-literal-name"),
});

transport_scenarios!(source_tracking_updates, run_scenario, {
    track_up_to_date: ("track-up-to-date"),
    track_at_head_dirty: ("track-at-head-dirty"),
    track_fast_forward: ("track-fast-forward"),
    track_local_divergent: ("track-local-divergent"),
    track_tracked_divergent: ("track-tracked-divergent"),
    track_deleted: ("track-deleted"),
    track_remote_rewind: ("track-remote-rewind"),
});

transport_scenarios!(source_tracking_refusals, run_scenario, {
    track_source_mismatch: ("track-source-mismatch"),
    track_source_rewrite: ("track-source-rewrite"),
    track_ambiguous: ("track-ambiguous"),
    track_pending_git: ("track-pending-git"),
    track_pending_remote: ("track-pending-remote"),
    track_local_conflict: ("track-local-conflict"),
});

transport_scenarios!(source_tracking_guards, run_scenario, {
    track_auto: ("track-auto"),
    track_auto_conflict: ("track-auto-conflict"),
    track_other_remote: ("track-other-remote"),
    track_immutable: ("track-immutable"),
    track_ignored: ("track-ignored"),
});

transport_scenarios!(source_tracking_pending_refs, run_scenario, {
    track_pending_delete: ("track-pending-delete"),
    track_multiple_urls: ("track-multiple-urls"),
    track_symbolic_remote: ("track-symbolic-remote"),
});

transport_scenarios!(pending_import_source_refs, run_scenario, {
    pending_before_source_delete: ("pending-before-source-delete"),
    pending_before_source_rewind: ("pending-before-source-rewind"),
    pending_before_source_update: ("pending-before-source-update"),
});

transport_scenarios!(pending_import_fallback_refs, run_scenario, {
    pending_before_fallback_delete: ("pending-before-fallback-delete"),
    pending_before_fallback_rewind: ("pending-before-fallback-rewind"),
    pending_before_fallback_update: ("pending-before-fallback-update"),
});

transport_scenarios!(pending_import_after_fetch, run_scenario, {
    pending_first_source_delete: ("pending-first-source-delete"),
    pending_first_source_rewind: ("pending-first-source-rewind"),
    pending_first_source_update: ("pending-first-source-update"),
    pending_first_fallback_delete: ("pending-first-fallback-delete"),
    pending_first_fallback_rewind: ("pending-first-fallback-rewind"),
    pending_first_fallback_update: ("pending-first-fallback-update"),
});

transport_scenarios!(pending_import_after_source_publication, run_scenario, {
    pending_second_source_delete: ("pending-second-source-delete"),
    pending_second_source_rewind: ("pending-second-source-rewind"),
    pending_second_source_update: ("pending-second-source-update"),
});

transport_scenarios!(pending_import_local_refs, run_scenario, {
    pending_before_local_delete: ("pending-before-local-delete"),
    pending_before_local_rewind: ("pending-before-local-rewind"),
    pending_before_local_update: ("pending-before-local-update"),
    pending_before_tag_delete: ("pending-before-tag-delete"),
    pending_before_tag_rewind: ("pending-before-tag-rewind"),
    pending_before_tag_update: ("pending-before-tag-update"),
});

transport_scenarios!(pending_import_new_refs, run_scenario, {
    pending_before_newlocal_update: ("pending-before-newlocal-update"),
    pending_before_newremote_update: ("pending-before-newremote-update"),
    pending_before_newtag_update: ("pending-before-newtag-update"),
});

fn jj(root: &Path, args: &[&str]) -> Result<String> {
    output(
        Command::new("jj")
            .current_dir(root)
            .arg("--no-pager")
            .args(args),
        b"",
    )
}
fn git(client: &Client, args: &[&str]) -> Result<String> {
    output(
        Command::new("git")
            .current_dir(&client.root)
            .arg("--no-pager")
            .arg("--git-dir")
            .arg(&client.git_dir)
            .args(args),
        b"",
    )
}

fn assert_review_checkout(client: &Client, head: &str, immutable: bool) -> Result<()> {
    let actual = client.checkout_commit()?;
    if immutable {
        assert_ne!(actual, head);
        assert_eq!(
            jj(
                &client.root,
                &[
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    &actual,
                    "--no-graph",
                    "-T",
                    "parents.map(|p| p.commit_id()).join(\" \") ++ \"\\t\" ++ empty ++ \"\\t\" ++ conflict",
                ],
            )?,
            format!("{head}\ttrue\tfalse")
        );
        assert_eq!(
            jj(
                &client.root,
                &[
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    head,
                    "--no-graph",
                    "-T",
                    "immutable"
                ]
            )?,
            "true",
            "review checkout must not override immutability"
        );
    } else {
        assert_eq!(actual, head);
    }
    Ok(())
}

fn ignored_checkout(
    client: &Client,
    mut pr: zed_pr_review::model::PullRequest,
    fixture: &Path,
    scenario: &str,
) -> Result<()> {
    let immutable = scenario.ends_with("-immutable");
    let scenario = scenario.strip_suffix("-immutable").unwrap_or(scenario);
    let root = &client.root;
    client.fetch_pr(&pr)?;
    let initial = client.checkout_commit()?;
    // Construct the target before any private files exist. Only ignore files are added.
    jj(root, &["new", &pr.head_sha, "-m", "Target ignore rules"])?;
    let nested = scenario.starts_with("ignore-nested-");
    let batched = scenario.starts_with("ignore-batched-");
    let target_rules = match scenario {
        "ignore-kept" | "ignore-late-file" => ".env\n",
        "ignore-negated" => ".env\n!.env\n",
        "ignore-node-modules" => ".env\nnode_modules/\n",
        "ignore-node-modules-contents" => ".env\nnode_modules/*\n",
        "ignore-batched-directories" => ".env\ncache/*/\n",
        "ignore-batched-leaves" => ".env\n*.pyc\n",
        _ => "",
    };
    if !target_rules.is_empty() {
        fs::write(root.join(".gitignore"), target_rules)?;
    }
    if scenario == "ignore-nested-kept" {
        fs::create_dir(root.join("config"))?;
        fs::write(root.join("config/.gitignore"), ".env\n")?;
    }
    jj(root, &["status"])?;
    pr.head_sha = client.checkout_commit()?;
    jj(root, &["bookmark", "create", "review-target"])?;
    let target_commit = git(client, &["cat-file", "commit", &pr.head_sha])?;
    jj(root, &["edit", &initial])?;
    if immutable {
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(fixture.join("jj.toml"))?;
        writeln!(
            config,
            "[revset-aliases]\n'immutable_heads()' = '{}'",
            pr.head_sha
        )?;
    }
    fs::write(
        root.join(".gitignore"),
        if batched {
            ".env\nscratch/\nnode_modules/\ncache/\n"
        } else if nested || scenario.ends_with("excludes") || scenario == "ignore-core-changed" {
            "scratch/\nnode_modules/\n"
        } else {
            ".env\nscratch/\nnode_modules/\n"
        },
    )?;
    if nested {
        fs::create_dir_all(root.join("config"))?;
        fs::write(root.join("config/.gitignore"), ".env\n")?;
    }
    if scenario.ends_with("excludes") {
        fs::create_dir_all(client.git_dir.join("info"))?;
        fs::write(client.git_dir.join("info/exclude"), ".env\n")?;
        fs::write(fixture.join("global-ignore"), "global-private\n")?;
        let global_ignore = if scenario == "ignore-relative-excludes" {
            PathBuf::from("../global-ignore")
        } else {
            fixture.join("global-ignore")
        };
        git(
            client,
            &[
                "config",
                "core.excludesFile",
                global_ignore.to_str().unwrap(),
            ],
        )?;
    }
    if scenario == "ignore-core-changed" {
        fs::write(root.join(".external-ignore"), ".env\n")?;
        git(
            client,
            &[
                "config",
                "core.excludesFile",
                root.join(".external-ignore").to_str().unwrap(),
            ],
        )?;
    }
    fs::write(root.join("keep.txt"), "original local work\n")?;
    jj(root, &["status"])?;
    let previous = client.checkout_commit()?;
    let previous_commit = git(client, &["cat-file", "commit", &previous])?;
    let secret_path = if nested { "config/.env" } else { ".env" };
    let secret = format!("private fixture token for {scenario}\n");
    fs::write(root.join(secret_path), &secret)?;
    let mut private_files = vec![(secret_path.to_owned(), secret)];
    let mut batched_paths = Vec::new();
    let batched_contents = "private batched cache contents\n";
    if batched {
        for index in 0..128 {
            let path = format!("cache/package {index}\nname/private data\n.pyc");
            fs::create_dir_all(root.join(&path).parent().unwrap())?;
            fs::write(root.join(&path), batched_contents)?;
            batched_paths.push(path);
        }
        // All siblings share these bytes; one object-store check covers them all.
        private_files.push((batched_paths[0].clone(), batched_contents.into()));
    }
    if scenario.starts_with("ignore-node-modules") {
        fs::create_dir_all(root.join("node_modules/package"))?;
        fs::write(
            root.join("node_modules/package/private"),
            "private dependency data\n",
        )?;
        private_files.push((
            "node_modules/package/private".into(),
            "private dependency data\n".into(),
        ));
    }
    if scenario.ends_with("excludes") {
        fs::write(
            root.join("global-private"),
            "globally ignored private data\n",
        )?;
        private_files.push((
            "global-private".into(),
            "globally ignored private data\n".into(),
        ));
    }
    // Source ignores scratch/; target does not. This retained local ignore file must
    // keep the tool's state private without treating all previously ignored dirs as unsafe.
    fs::create_dir_all(root.join("scratch/zed-pr-review"))?;
    fs::write(root.join("scratch/zed-pr-review/.gitignore"), "*\n")?;
    fs::write(
        root.join("scratch/zed-pr-review/state"),
        "private review state\n",
    )?;
    private_files.push((
        "scratch/zed-pr-review/state".into(),
        "private review state\n".into(),
    ));
    if scenario == "ignore-late-file" {
        // Simulate a file arriving after the guard, just after jj edit/new finishes. Final
        // verification must not snapshot it, even with the default auto-track=all().
        let real_jj = PathBuf::from(std::env::var_os("ZPR_REAL_JJ").unwrap());
        let wrapper = fixture.join("bin/jj");
        support::install(
            &wrapper,
            &json!({
                "tool": "transport-jj", "real_jj": real_jj,
                "after_success": {
                    "commands": ["edit", "new"],
                    "path": ".late-private", "contents": "late private data\n",
                },
            }),
        )?;
        private_files.push((".late-private".into(), "late private data\n".into()));
    }
    let rejected = matches!(
        scenario,
        "ignore-removed" | "ignore-negated" | "ignore-nested-removed" | "ignore-core-changed"
    );
    let git_log = fixture.join("git-calls.jsonl");
    let log_start = if batched {
        Some(fs::read_to_string(&git_log)?.len())
    } else {
        None
    };
    let result = client.checkout_pr(&pr);
    if let Some(start) = log_start {
        // Count only checkout queries, not setup or the object-store checks below.
        let log = fs::read_to_string(&git_log)?;
        let calls = log[start..]
            .lines()
            .map(serde_json::from_str::<Vec<String>>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let queries: Vec<_> = calls
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "check-ignore"))
            .collect();
        assert!(
            !queries.is_empty() && queries.len() < 10,
            "{scenario}: expected bounded batches for 128 siblings, got {} queries: {queries:?}",
            queries.len()
        );
        for args in queries {
            for flag in ["--no-index", "-z", "--stdin"] {
                assert!(
                    args.iter().any(|arg| arg == flag),
                    "missing {flag}: {args:?}"
                );
            }
        }
    }
    if rejected {
        assert!(result.is_err(), "must refuse newly exposed private files");
        assert_eq!(
            client.checkout_commit()?,
            previous,
            "refuse BEFORE changing @"
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains(if scenario == "ignore-core-changed" {
                "ignore file changes"
            } else {
                "would no longer be ignored"
            }),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(root.join("keep.txt"))?,
            "original local work\n"
        );
    } else {
        assert_eq!(result?, previous);
        assert_review_checkout(client, &pr.head_sha, immutable)?;
        client.ensure_checkout(&pr)?;
    }
    assert_eq!(
        git(client, &["cat-file", "commit", &previous])?,
        previous_commit
    );
    assert_eq!(
        git(client, &["cat-file", "commit", &pr.head_sha])?,
        target_commit
    );
    assert_eq!(
        jj(
            root,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "review-target",
                "--no-graph",
                "-T",
                "commit_id"
            ]
        )?,
        pr.head_sha
    );
    assert_eq!(
        client.blob(&previous, "keep.txt")?,
        b"original local work\n"
    );
    for path in batched_paths {
        assert_eq!(
            fs::read_to_string(root.join(&path))?,
            batched_contents,
            "{path:?}"
        );
    }
    for (path, contents) in private_files {
        assert_eq!(
            fs::read_to_string(root.join(&path))?,
            contents,
            "private contents changed: {path}"
        );
        // Check the object store, not merely reachable commits: even an abandoned
        // snapshot of a private blob is a failure. hash-object deliberately has no -w.
        let hash = output(
            Command::new("git")
                .current_dir(root)
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&client.git_dir)
                .args(["hash-object", "--stdin"]),
            contents.as_bytes(),
        )?;
        let exists = Command::new("git")
            .current_dir(root)
            .arg("--no-pager")
            .arg("--git-dir")
            .arg(&client.git_dir)
            .args(["cat-file", "-e", &hash])
            .output()?;
        assert!(
            !exists.status.success(),
            "private file entered the Git object store: {path}"
        );
    }
    Ok(())
}

fn source_listing(client: &Client) -> Result<String> {
    jj(
        &client.root,
        &[
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--all-remotes",
            "exact:feature/topic",
            "-T",
            r#"name ++ "\t" ++ remote ++ "\t" ++ conflict ++ "\t" ++ if(normal_target, normal_target.commit_id()) ++ "\t" ++ tracked ++ "\n""#,
        ],
    )
}

fn tracking_literal_name(client: &Client, fixture: &Path) -> Result<()> {
    let branch = "feature/topic|other";
    let mut control: Value = serde_json::from_slice(&fs::read(fixture.join("control.json"))?)?;
    control["head"]["repo"]["full_name"] = json!("upstream/project");
    control["head"]["ref"] = json!(branch);
    fs::write(fixture.join("control.json"), serde_json::to_vec(&control)?)?;
    let pr = client.read_pr(42)?;
    output(
        Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
            .arg("--no-pager")
            .arg("--git-dir")
            .arg(fixture.join("base.git"))
            .args(["update-ref", &format!("refs/heads/{branch}"), &pr.head_sha]),
        b"",
    )?;
    git(client, &["remote", "add", "origin", URL])?;
    jj(&client.root, &["bookmark", "create", "other", "-r", "@"])?;
    git(
        client,
        &["update-ref", "refs/remotes/origin/other", &pr.base_sha],
    )?;
    jj(&client.root, &["--ignore-working-copy", "git", "import"])?;
    let before = client.checkout_commit()?;
    fs::write(client.root.join("keep.txt"), "dirty work\n")?;
    client.fetch_pr(&pr)?;
    assert_eq!(client.checkout_commit()?, before);
    assert_eq!(
        jj(
            &client.root,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "other",
                "--no-graph",
                "-T",
                "commit_id"
            ]
        )?,
        before
    );
    let listing = jj(
        &client.root,
        &[
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--all-remotes",
            "-T",
            "name ++ \"\\t\" ++ remote ++ \"\\t\" ++ tracked ++ \"\\n\"",
        ],
    )?;
    assert!(
        listing.contains(&format!("\"{branch}\"\torigin\ttrue")),
        "{listing}"
    );
    assert!(listing.contains("other\torigin\tfalse"), "{listing}");
    assert_eq!(
        git(
            client,
            &["rev-parse", &format!("refs/remotes/origin/{branch}")]
        )?,
        pr.head_sha
    );
    Ok(())
}

fn tracking_checkout(client: &Client, fixture: &Path, scenario: &str) -> Result<()> {
    let directory = &client.root;
    let initial_pr = client.read_pr(42)?;
    // Most creation cases start without PR objects. Other cases need local objects
    // to prepare bookmarks before configuring a remote, without any network access.
    if !matches!(
        scenario,
        "track-same-repo" | "track-fork-absent" | "track-wrong-host"
    ) {
        client.fetch_pr(&initial_pr)?;
    }
    if scenario == "track-literal-name" {
        return tracking_literal_name(client, fixture);
    }
    jj(directory, &["bookmark", "create", "user-kept", "-r", "@"])?;
    let common = std::env::var("ZPR_COMMON")?;
    let fork = matches!(scenario, "track-fork-present" | "track-fork-absent");
    let absent = matches!(scenario, "track-fork-absent" | "track-wrong-host");
    let mut control: Value = serde_json::from_slice(&fs::read(fixture.join("control.json"))?)?;
    if !fork {
        control["head"]["repo"]["full_name"] = json!("upstream/project");
    }
    fs::write(fixture.join("control.json"), serde_json::to_vec(&control)?)?;
    let pr = client.read_pr(42)?;
    let source_name = if fork { "contributor.remote" } else { "origin" };
    let mut source_store = fixture.join("base.git");
    if scenario == "track-fork-present" {
        source_store = fixture.join("fork.git");
        output(
            Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
                .args(["--no-pager", "clone", "--bare"])
                .arg(fixture.join("base.git"))
                .arg(&source_store),
            b"",
        )?;
        git(
            client,
            &[
                "remote",
                "add",
                source_name,
                "git@github.com:contributor/fork.git",
            ],
        )?;
    }
    git(
        client,
        &[
            "remote",
            "add",
            "origin",
            if scenario == "track-wrong-host" {
                "https://github.example/upstream/project.git"
            } else {
                URL
            },
        ],
    )?;
    if !absent && scenario != "track-source-mismatch" {
        output(
            Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&source_store)
                .args(["update-ref", "refs/heads/feature/topic", &pr.head_sha]),
            b"",
        )?;
    }
    if scenario == "track-fork-present" {
        // The identically named base-repository branch is a different commit. Its
        // existing remote bookmark must not get tracked or confused with the fork.
        git(
            client,
            &[
                "update-ref",
                "refs/remotes/origin/feature/topic",
                &pr.base_sha,
            ],
        )?;
        jj(directory, &["--ignore-working-copy", "git", "import"])?;
    }
    // These broad settings must never widen the HTTPS fetch or change any other ref.
    git(
        client,
        &["config", &format!("remote.{source_name}.tagOpt"), "--tags"],
    )?;
    git(
        client,
        &[
            "config",
            &format!("remote.{source_name}.fetch"),
            "+refs/heads/feature/topic:refs/heads/user-kept",
        ],
    )?;
    git(client, &["config", "fetch.prune", "true"])?;
    git(client, &["config", "fetch.pruneTags", "true"])?;
    let remote_ref = format!("refs/remotes/{source_name}/feature/topic");
    let tracked = matches!(
        scenario,
        "track-up-to-date"
            | "track-at-head-dirty"
            | "track-fast-forward"
            | "track-tracked-divergent"
            | "track-deleted"
            | "track-remote-rewind"
            | "track-pending-remote"
            | "track-pending-delete"
            | "track-source-mismatch"
            | "track-other-remote"
    );
    if tracked {
        let old = match scenario {
            "track-up-to-date" | "track-at-head-dirty" => &pr.head_sha,
            "track-remote-rewind" => &pr.base_sha,
            _ => &common,
        };
        git(client, &["update-ref", &remote_ref, old])?;
        jj(directory, &["--ignore-working-copy", "git", "import"])?;
        jj(
            directory,
            &[
                "--ignore-working-copy",
                "bookmark",
                "track",
                &format!("feature/topic@{source_name}"),
            ],
        )?;
        match scenario {
            "track-at-head-dirty" => {
                jj(directory, &["edit", &pr.head_sha])?;
            }
            "track-fast-forward" => {
                jj(
                    directory,
                    &["new", &common, "-m", "Keep this descendant unchanged"],
                )?;
            }
            "track-tracked-divergent" => {
                jj(
                    directory,
                    &["bookmark", "set", "feature/topic", "-r", &pr.base_sha],
                )?;
            }
            "track-deleted" => {
                jj(directory, &["bookmark", "delete", "feature/topic"])?;
            }
            "track-pending-remote" => {
                git(client, &["update-ref", &remote_ref, &pr.head_sha])?;
            }
            "track-pending-delete" => {
                // Export explicitly in non-colocated stores, then leave a deletion
                // pending in Git. It must not be imported and silently resurrected.
                jj(directory, &["--ignore-working-copy", "git", "export"])?;
                git(client, &["update-ref", "-d", "refs/heads/feature/topic"])?;
            }
            "track-other-remote" => {
                git(
                    client,
                    &[
                        "remote",
                        "add",
                        "other",
                        "https://github.com/somewhere/else.git",
                    ],
                )?;
                git(
                    client,
                    &["update-ref", "refs/remotes/other/feature/topic", &common],
                )?;
                jj(directory, &["--ignore-working-copy", "git", "import"])?;
                jj(directory, &["bookmark", "track", "feature/topic@other"])?;
            }
            _ => {}
        }
    }
    if matches!(
        scenario,
        "track-local-equal"
            | "track-local-divergent"
            | "track-local-conflict"
            | "track-auto-conflict"
    ) {
        jj(
            directory,
            &[
                "bookmark",
                "create",
                "feature/topic",
                "-r",
                if matches!(scenario, "track-local-divergent" | "track-auto-conflict") {
                    "@"
                } else {
                    &pr.head_sha
                },
            ],
        )?;
    }
    if scenario == "track-local-conflict" {
        let op = jj(
            directory,
            &["op", "log", "-n", "1", "--no-graph", "-T", "id"],
        )?;
        jj(
            directory,
            &[
                "bookmark",
                "set",
                "feature/topic",
                "-r",
                &pr.base_sha,
                "--allow-backwards",
            ],
        )?;
        jj(
            directory,
            &[
                "--at-op",
                &op,
                "bookmark",
                "set",
                "feature/topic",
                "-r",
                "@",
                "--allow-backwards",
            ],
        )?;
        assert!(source_listing(client)?.contains("\ttrue\t"));
    }
    if scenario == "track-pending-git" {
        git(
            client,
            &["update-ref", "refs/heads/feature/topic", &pr.head_sha],
        )?;
    }
    if scenario == "track-multiple-urls" {
        git(
            client,
            &[
                "config",
                "--add",
                "remote.origin.url",
                "https://github.com/other/repo.git",
            ],
        )?;
    }
    if scenario == "track-symbolic-remote" {
        git(
            client,
            &["symbolic-ref", &remote_ref, "refs/heads/user-kept"],
        )?;
    }
    if scenario == "track-ambiguous" {
        git(
            client,
            &[
                "remote",
                "add",
                "duplicate",
                "ssh://git@github.com/upstream/project.git",
            ],
        )?;
    }
    if scenario == "track-source-rewrite" {
        // A source-only rewrite cannot affect the base PR fetch.
        let mut control = control.clone();
        control["head"]["repo"]["full_name"] = json!("contributor/fork");
        fs::write(fixture.join("control.json"), serde_json::to_vec(&control)?)?;
        git(
            client,
            &[
                "remote",
                "add",
                "fork",
                "https://github.com/contributor/fork.git",
            ],
        )?;
        git(
            client,
            &[
                "config",
                "url.file:///never-fetch.insteadOf",
                "https://github.com/contributor/fork.git",
            ],
        )?;
    }
    if matches!(scenario, "track-auto" | "track-auto-conflict") {
        git(
            client,
            &[
                "remote",
                "add",
                "unrelated.remote",
                "https://github.com/unrelated/repo.git",
            ],
        )?;
        git(
            client,
            &[
                "update-ref",
                "refs/remotes/unrelated.remote/do-not-track",
                &pr.base_sha,
            ],
        )?;
        // Already-known, untracked refs remain untracked even with broad rules.
        // New pending unrelated refs are covered by pending_import_new_refs.
        jj(directory, &["--ignore-working-copy", "git", "import"])?;
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(fixture.join("jj.toml"))?;
        writeln!(
            config,
            "[git]\nauto-local-bookmark = true\n[remotes.origin]\nauto-track-bookmarks = '*'\nauto-track-created-bookmarks = '*'\n[remotes.\"unrelated.remote\"]\nauto-track-bookmarks = '*'\n"
        )?;
    }
    if scenario == "track-immutable" {
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(fixture.join("jj.toml"))?;
        writeln!(
            config,
            "[revset-aliases]\n'immutable_heads()' = '{}'",
            pr.head_sha
        )?;
    }
    if scenario == "track-ignored" {
        fs::write(directory.join(".gitignore"), "private-key\n")?;
        jj(directory, &["status"])?;
        fs::write(directory.join("private-key"), "do not snapshot this\n")?;
    }
    let pr = client.read_pr(42)?;
    let recorded = client.checkout_commit()?;
    let bookmarks_before = source_listing(client)?;
    let refs_before = git(
        client,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/remotes",
            "refs/heads/feature",
        ],
    )?;
    let kept = jj(
        directory,
        &[
            "--ignore-working-copy",
            "log",
            "-r",
            "user-kept",
            "--no-graph",
            "-T",
            "commit_id",
        ],
    )?;
    let jj_config_before = fs::read(fixture.join("jj.toml"))?;
    fs::write(directory.join("keep.txt"), "unsnapshotted local changes\n")?;
    let hooks = fixture.join("tracking-hooks");
    fs::create_dir(&hooks)?;
    let hook = hooks.join("reference-transaction");
    support::install(
        &hook,
        &json!({"tool": "transport-hook", "marker": fixture.join("tracking-hook-ran")}),
    )?;
    git(
        client,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    )?;
    let config_before = fs::read(client.git_dir.join("config"))?;
    let jj_calls_before = fs::read_to_string(fixture.join("jj-calls.jsonl"))?
        .lines()
        .count();
    let result = client.fetch_pr(&pr);
    assert!(!fixture.join("tracking-hook-ran").exists());
    let jj_calls = fs::read_to_string(fixture.join("jj-calls.jsonl"))?;
    for call in jj_calls.lines().skip(jj_calls_before) {
        let args: Vec<String> = serde_json::from_str(call)?;
        assert!(
            args.iter().any(|a| a == "--ignore-working-copy"),
            "{args:?}"
        );
        assert!(
            !args.iter().any(
                |a| ["edit", "new", "abandon", "rebase", "--ignore-immutable"]
                    .contains(&a.as_str())
            ),
            "{args:?}"
        );
    }
    assert_eq!(
        client.checkout_commit()?,
        recorded,
        "fetch must neither snapshot nor rebase @"
    );
    assert_eq!(
        fs::read_to_string(directory.join("keep.txt"))?,
        "unsnapshotted local changes\n"
    );
    assert_eq!(
        fs::read(client.git_dir.join("config"))?,
        config_before,
        "never modify remotes"
    );
    assert_eq!(
        fs::read(fixture.join("jj.toml"))?,
        jj_config_before,
        "config overrides must be temporary"
    );
    let refusal = match scenario {
        "track-source-mismatch" => Some("does not match the pinned PR head"),
        "track-source-rewrite" => Some("URL rewriting"),
        "track-ambiguous" => Some("Multiple remotes"),
        "track-local-divergent" | "track-tracked-divergent" | "track-auto-conflict" => {
            Some("differs from the pinned head")
        }
        "track-deleted" => Some("deleted locally"),
        "track-remote-rewind" => Some("not a fast-forward"),
        "track-pending-git" | "track-pending-remote" | "track-pending-delete" => {
            Some("pending Git changes")
        }
        "track-multiple-urls" => Some("multiple URLs"),
        "track-symbolic-remote" => Some("symbolic source ref"),
        "track-local-conflict" => Some("conflicted"),
        "track-other-remote" => Some("already tracks another remote"),
        _ => None,
    };
    if let Some(message) = refusal {
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains(message), "{scenario}: {error}");
        assert_eq!(
            source_listing(client)?,
            bookmarks_before,
            "refusal must preserve source bookmarks"
        );
        assert_eq!(
            git(
                client,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/remotes",
                    "refs/heads/feature"
                ]
            )?,
            refs_before
        );
        return Ok(());
    }
    result?;
    assert_eq!(
        jj(
            directory,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "user-kept",
                "--no-graph",
                "-T",
                "commit_id"
            ]
        )?,
        kept
    );
    if absent {
        assert!(source_listing(client)?.is_empty());
        assert_eq!(
            git(
                client,
                &["for-each-ref", "--format=%(refname)", "refs/remotes"]
            )?,
            ""
        );
    } else {
        let listing = source_listing(client)?;
        assert!(listing.lines().any(|line| line.starts_with(&format!("feature/topic\t\tfalse\t{}\t", pr.head_sha))), "{listing}");
        assert!(
            listing.contains(&format!(
                "feature/topic\t{source_name}\tfalse\t{}\ttrue",
                pr.head_sha
            )),
            "{listing}"
        );
        assert_eq!(git(client, &["rev-parse", &remote_ref])?, pr.head_sha);
        assert!(
            git(
                client,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/remotes/origin/main"
                ]
            )?
            .is_empty()
        );
        if fork {
            assert_eq!(
                git(client, &["rev-parse", "refs/remotes/origin/feature/topic"])?,
                pr.base_sha
            );
            assert!(
                listing.contains(&format!(
                    "feature/topic\torigin\tfalse\t{}\tfalse",
                    pr.base_sha
                )),
                "{listing}"
            );
        }
        if scenario == "track-auto" {
            let untracked = jj(
                directory,
                &[
                    "--ignore-working-copy",
                    "bookmark",
                    "list",
                    "--all-remotes",
                    "exact:do-not-track",
                    "-T",
                    "remote ++ \"\\t\" ++ tracked ++ \"\\n\"",
                ],
            )?;
            assert_eq!(untracked, "unrelated.remote\tfalse");
        }
        // Already-tracked, up-to-date fetches are idempotent apart from fresh private refs.
        client.fetch_pr(&pr)?;
        assert_eq!(source_listing(client)?, listing);
        assert_eq!(client.checkout_commit()?, recorded);
    }
    assert!(!client.git_dir.join("FETCH_HEAD").exists());
    let checkout = client.checkout_pr(&pr);
    if scenario == "track-ignored" {
        let error = format!("{:#}", checkout.unwrap_err());
        assert!(error.contains("ignored"), "{error}");
        assert_ne!(client.checkout_commit()?, pr.head_sha);
        assert_eq!(
            fs::read_to_string(directory.join("private-key"))?,
            "do not snapshot this\n"
        );
    } else {
        let previous = checkout?;
        assert_review_checkout(client, &pr.head_sha, scenario == "track-immutable")?;
        client.ensure_checkout(&pr)?;
        assert_eq!(
            client.blob(&previous, "keep.txt")?,
            b"unsnapshotted local changes\n"
        );
        if scenario == "track-at-head-dirty" {
            assert_ne!(previous, pr.head_sha);
            assert_eq!(
                jj(
                    directory,
                    &[
                        "--ignore-working-copy",
                        "log",
                        "-r",
                        "feature/topic",
                        "--no-graph",
                        "-T",
                        "commit_id"
                    ]
                )?,
                previous,
                "snapshotting dirty work must retain its local bookmark, not reset it to the PR head"
            );
            assert_eq!(git(client, &["rev-parse", &remote_ref])?, pr.head_sha);
        }
    }
    Ok(())
}

fn pending_import_checkout(client: &Client, fixture: &Path, scenario: &str) -> Result<()> {
    let parts: Vec<_> = scenario.split('-').collect();
    let stage = parts[1];
    let kind = parts[2];
    let source = kind == "source";
    let action = parts[3];
    let root = &client.root;
    client.fetch_pr(&client.read_pr(42)?)?;
    if stage == "first" {
        // This race is injected by the Git fetch shim. Clear retained transport
        // refs so the seeded PR cannot skip that fetch through a cache hit.
        let cached_refs = git(
            client,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/zed-pr-review-prefetch/",
            ],
        )?;
        for reference in cached_refs.lines() {
            git(client, &["update-ref", "--no-deref", "-d", reference])?;
        }
    }
    let recorded = client.checkout_commit()?;
    let mut commits = Vec::new();
    for (parent, description) in [
        ("root()", "Unrelated parent"),
        ("@", "Unrelated tip"),
        ("@", "Unrelated descendant"),
    ] {
        jj(root, &["new", parent, "-m", description])?;
        commits.push(client.checkout_commit()?);
    }
    jj(root, &["edit", &recorded])?;
    git(
        client,
        &[
            "remote",
            "add",
            "unrelated",
            "https://github.com/other/repo.git",
        ],
    )?;
    let reference = "refs/remotes/unrelated/kept/topic";
    git(client, &["update-ref", reference, &commits[1]])?;
    jj(root, &["--ignore-working-copy", "git", "import"])?;
    jj(
        root,
        &[
            "--ignore-working-copy",
            "bookmark",
            "track",
            "kept/topic@unrelated",
        ],
    )?;
    jj(root, &["--ignore-working-copy", "git", "export"])?;
    let reference = match kind {
        "local" => "refs/heads/kept/topic",
        "newlocal" => "refs/heads/new-topic",
        "newremote" => "refs/remotes/unrelated/new-topic",
        "tag" | "newtag" => "refs/tags/kept-tag",
        _ => reference,
    };
    if kind == "tag" {
        git(
            client,
            &["tag", "-a", "kept-tag", &commits[1], "-m", "Annotated tag"],
        )?;
        jj(root, &["--ignore-working-copy", "git", "import"])?;
    }
    if source {
        let mut control: Value = serde_json::from_slice(&fs::read(fixture.join("control.json"))?)?;
        control["head"]["repo"]["full_name"] = json!("upstream/project");
        fs::write(fixture.join("control.json"), serde_json::to_vec(&control)?)?;
        git(client, &["remote", "add", "origin", URL])?;
        output(
            Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(fixture.join("base.git"))
                .args([
                    "update-ref",
                    "refs/heads/feature/topic",
                    &client.read_pr(42)?.head_sha,
                ]),
            b"",
        )?;
    }
    let pr = client.read_pr(42)?;
    let user_bookmarks = || {
        jj(
            root,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "--all-remotes",
                "-T",
                r#"if(!name.starts_with("zed-pr-review/"), json(name) ++ "\t" ++ json(remote) ++ "\t" ++ conflict ++ "\t" ++ if(normal_target, normal_target.commit_id()) ++ "\t" ++ tracked ++ "\n")"#,
            ],
        )
    };
    let visible_commits = || {
        jj(
            root,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "all()",
                "--no-graph",
                "-T",
                r#"commit_id ++ " " ++ parents.map(|p| p.commit_id()).join(" ") ++ "\n""#,
            ],
        )
    };
    let bookmarks_before = user_bookmarks()?;
    let commits_before = visible_commits()?;
    let tags_before = jj(
        root,
        &[
            "--ignore-working-copy",
            "tag",
            "list",
            "--all-remotes",
            "-T",
            "json(self) ++ \"\\n\"",
        ],
    )?;
    let target = match action {
        "delete" => None,
        "rewind" => Some(&commits[0]),
        "update" => Some(&commits[2]),
        _ => unreachable!(),
    };
    if stage == "before" {
        if let Some(target) = target {
            git(client, &["update-ref", reference, target])?;
        } else {
            git(client, &["update-ref", "-d", reference])?;
        }
    } else {
        fs::write(
            fixture.join("pending-ref.json"),
            serde_json::to_vec(&json!({
                "stage": stage, "reference": reference, "target": target, "git_dir": client.git_dir,
            }))?,
        )?;
    }
    let local_before = git(
        client,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/kept/topic",
        ],
    )?;
    let pending_ref = target
        .map(|t| format!("{reference} {t}"))
        .unwrap_or_default();
    fs::write(root.join("keep.txt"), "unsnapshotted work\n")?;
    let calls_before = fs::read_to_string(fixture.join("jj-calls.jsonl"))?
        .lines()
        .count();
    let result = client.fetch_pr(&pr);
    assert!(
        result.is_err(),
        "fetch imported an unrelated {action}: before={bookmarks_before}, after={}",
        user_bookmarks()?
    );
    let error = format!("{:#}", result.unwrap_err());
    assert!(
        error.contains("pending Git changes") && error.contains(reference),
        "{error}"
    );
    assert_eq!(
        user_bookmarks()?,
        bookmarks_before,
        "never move unrelated tracking bookmarks"
    );
    assert_eq!(
        visible_commits()?,
        commits_before,
        "never abandon or rebase unrelated commits"
    );
    assert_eq!(client.checkout_commit()?, recorded);
    assert_eq!(
        fs::read_to_string(root.join("keep.txt"))?,
        "unsnapshotted work\n"
    );
    assert_eq!(
        git(
            client,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/kept/topic"
            ]
        )?,
        local_before
    );
    assert_eq!(
        jj(
            root,
            &[
                "--ignore-working-copy",
                "tag",
                "list",
                "--all-remotes",
                "-T",
                "json(self) ++ \"\\n\""
            ]
        )?,
        tags_before
    );
    if stage != "before" {
        assert!(fixture.join("injected-ref.json").exists());
    }
    assert_eq!(
        git(
            client,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                reference
            ]
        )?,
        pending_ref,
        "do not roll back the pending Git change"
    );
    let calls = fs::read_to_string(fixture.join("jj-calls.jsonl"))?;
    let imports = calls
        .lines()
        .skip(calls_before)
        .map(|call| serde_json::from_str::<Vec<String>>(call).unwrap())
        .filter(|args| args.windows(2).any(|a| a == ["git", "import"]))
        .count();
    assert_eq!(
        imports,
        usize::from(stage == "second"),
        "no global import may apply the unrelated change"
    );
    Ok(())
}

fn base_tip_checkout(
    client: &Client,
    pr: zed_pr_review::model::PullRequest,
    fixture: &Path,
    scenario: &str,
) -> Result<()> {
    let shallow = scenario.ends_with("-shallow");
    let scenario = scenario.strip_prefix("base-tip-").unwrap();
    let scenario = scenario.strip_suffix("-shallow").unwrap_or(scenario);
    let root = &client.root;
    let actual = fs::read_to_string(fixture.join("actual-base"))?;
    let pinned = pr.clone();
    assert_ne!(actual, pr.base_sha);
    let local_fetch = |args: &[&str]| {
        output(
            Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&client.git_dir)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "fetch",
                    "--no-write-fetch-head",
                ])
                .args(args),
            b"",
        )
    };
    let url = format!("file://{}", fixture.join("base.git").display());
    if ["rewind", "divergent"].contains(&scenario) {
        // Make the pinned object available so these fail on ancestry, not absence.
        local_fetch(&[
            &url,
            "refs/heads/feature/topic:refs/zed-pr-review-fixture/pinned-base",
        ])?;
        assert_eq!(git(client, &["cat-file", "-t", &pr.base_sha])?, "commit");
    }
    if shallow {
        local_fetch(&[
            "--depth=1",
            &url,
            "refs/pull/42/head:refs/zed-pr-review-fixture/head",
            "refs/heads/main:refs/zed-pr-review-fixture/base",
        ])?;
        assert_eq!(
            git(client, &["rev-parse", "--is-shallow-repository"])?,
            "true"
        );
    }
    if !["rewind", "divergent"].contains(&scenario) {
        assert!(git(client, &["cat-file", "-t", &pr.base_sha]).is_err());
    }
    fs::write(root.join("file.txt"), "local content before fetch\n")?;
    jj(root, &["status"])?;
    let recorded = client.checkout_commit()?;
    let user_ref = "refs/heads/user-kept";
    git(client, &["update-ref", user_ref, &recorded])?;
    jj(root, &["--ignore-working-copy", "git", "import"])?;
    let imported_refs = || {
        jj(
            root,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "--all-remotes",
                "-T",
                r#"if(remote == "git", name ++ "\t" ++ if(normal_target, normal_target.commit_id()) ++ "\n")"#,
            ],
        )
    };
    let imported_before = imported_refs()?;
    fs::write(root.join("file.txt"), "unsnapshotted tracked changes\n")?;
    fs::write(root.join("keep.txt"), "unsnapshotted local changes\n")?;
    // Even another ref under our private prefix must not gain blanket permission.
    let unrelated_ref = "refs/heads/zed-pr-review/unrelated/kept";
    if scenario == "unrelated-ref" {
        fs::write(
            fixture.join("pending-ref.json"),
            serde_json::to_vec(&json!({
                "stage": "first", "reference": unrelated_ref,
                "target": pr.head_sha, "git_dir": client.git_dir,
            }))?,
        )?;
    }
    let calls_before = fs::read_to_string(fixture.join("jj-calls.jsonl"))?
        .lines()
        .count();
    let result = client.fetch_pr(&pr);
    assert_eq!(
        client.checkout_commit()?,
        recorded,
        "fetch must not snapshot @"
    );
    assert_eq!(
        client.blob(&recorded, "file.txt")?,
        b"local content before fetch\n"
    );
    assert_eq!(
        fs::read(root.join("file.txt"))?,
        b"unsnapshotted tracked changes\n"
    );
    assert_eq!(
        fs::read(root.join("keep.txt"))?,
        b"unsnapshotted local changes\n"
    );
    assert_eq!(git(client, &["rev-parse", user_ref])?, recorded);
    assert_eq!(pr, pinned);
    assert_eq!(
        client.read_pr(42)?,
        pinned,
        "fetch must not replace the API pins"
    );
    let calls = fs::read_to_string(fixture.join("git-calls.jsonl"))?;
    let fetches: Vec<Vec<String>> = calls
        .lines()
        .map(|line| serde_json::from_str::<Vec<String>>(line).unwrap())
        .filter(|args| args.iter().any(|arg| arg == "fetch"))
        .collect();
    assert_eq!(fetches.len(), 1, "only the hardened PR fetch is allowed");
    assert_eq!(fetches[0].iter().any(|arg| arg == "--unshallow"), shallow);
    if scenario != "fast-forward" {
        let error = format!("{:#}", result.unwrap_err());
        if scenario == "unrelated-ref" {
            assert!(
                error.contains("pending Git changes") && error.contains(unrelated_ref),
                "{error}"
            );
            assert!(fixture.join("injected-ref.json").exists());
            assert_eq!(
                git(client, &["rev-parse", unrelated_ref])?,
                pr.head_sha,
                "do not roll back an unrelated pending ref"
            );
        } else {
            assert!(error.to_ascii_lowercase().contains("base"), "{error}");
            assert!(
                !error.contains("pending Git changes"),
                "must fail before import: {error}"
            );
            if scenario == "unavailable" {
                assert!(git(client, &["cat-file", "-t", &pr.base_sha]).is_err());
            } else {
                assert_eq!(git(client, &["cat-file", "-t", &pr.base_sha])?, "commit");
            }
        }
        assert_eq!(
            imported_refs()?,
            imported_before,
            "no refs may be imported on refusal"
        );
        let jj_calls = fs::read_to_string(fixture.join("jj-calls.jsonl"))?;
        for call in jj_calls.lines().skip(calls_before) {
            let args: Vec<String> = serde_json::from_str(call)?;
            assert!(
                !args.windows(2).any(|args| args == ["git", "import"]),
                "{args:?}"
            );
        }
        return Ok(());
    }
    result?;
    assert_eq!(
        git(client, &["rev-parse", "--is-shallow-repository"])?,
        "false"
    );
    assert_eq!(client.merge_base(&pr)?, std::env::var("ZPR_COMMON")?);
    assert_eq!(
        git(client, &["merge-base", &pr.head_sha, &actual])?,
        pr.head_sha,
        "using the newer tip would silently change the review diff"
    );
    assert_eq!(client.blob(&pr.base_sha, "file.txt")?, b"base diverged\n");
    assert_eq!(client.blob(&pr.head_sha, "file.txt")?, b"fork head\n");
    assert_eq!(
        jj(
            root,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                &format!("all() & {actual}"),
                "--no-graph",
                "-T",
                "commit_id"
            ]
        )?,
        actual,
        "the actual fetched base must be imported into jj"
    );
    let refs = git(
        client,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            "refs/heads/zed-pr-review/",
        ],
    )?;
    assert_eq!(refs.lines().count(), 2);
    let imported = imported_refs()?;
    for suffix in ["head", "base"] {
        let (reference, sha) = refs
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .find(|(reference, _)| reference.ends_with(&format!("/{suffix}")))
            .context("Missing fetched ref")?;
        assert_eq!(
            sha,
            if suffix == "head" {
                &pr.head_sha
            } else {
                &actual
            }
        );
        let name = reference.strip_prefix("refs/heads/").unwrap();
        assert!(
            imported
                .lines()
                .any(|line| line == format!("{name}\t{sha}")),
            "the import must authorize the exact fetched ref and SHA: {imported}"
        );
    }
    client.fetch_pr(&pr)?;
    let refs_again = git(
        client,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            "refs/heads/zed-pr-review/",
        ],
    )?;
    assert_eq!(refs_again.lines().count(), 4);
    for original in refs.lines() {
        assert!(refs_again.lines().any(|line| line == original));
    }
    assert_eq!(client.checkout_commit()?, recorded);
    assert_eq!(
        fs::read(root.join("file.txt"))?,
        b"unsnapshotted tracked changes\n"
    );
    assert_eq!(
        fs::read(root.join("keep.txt"))?,
        b"unsnapshotted local changes\n"
    );
    assert_eq!(client.read_pr(42)?, pinned);
    assert_eq!(client.merge_base(&pr)?, std::env::var("ZPR_COMMON")?);
    assert!(!client.git_dir.join("FETCH_HEAD").exists());
    let previous = client.checkout_pr(&pr)?;
    assert_review_checkout(client, &pr.head_sha, false)?;
    client.ensure_checkout(&pr)?;
    assert_eq!(fs::read(root.join("file.txt"))?, b"fork head\n");
    assert!(!root.join("keep.txt").exists());
    assert_ne!(previous, recorded);
    assert_eq!(
        client.blob(&previous, "file.txt")?,
        b"unsnapshotted tracked changes\n"
    );
    assert_eq!(
        client.blob(&previous, "keep.txt")?,
        b"unsnapshotted local changes\n"
    );
    jj(root, &["edit", &previous])?;
    assert_eq!(
        fs::read(root.join("file.txt"))?,
        b"unsnapshotted tracked changes\n"
    );
    assert_eq!(
        fs::read(root.join("keep.txt"))?,
        b"unsnapshotted local changes\n"
    );
    Ok(())
}

#[test]
fn checkout_child() -> Result<()> {
    let Ok(scenario) = std::env::var("ZPR_SCENARIO") else {
        return Ok(());
    };
    let directory = std::env::current_dir()?;
    let fixture = PathBuf::from(std::env::var_os("ZPR_FIXTURE").unwrap());
    let (client, pr) = Client::resolve(&directory, "42", None)?;
    assert_eq!(workspace_root(&directory)?, directory);
    assert_ne!(client.checkout_commit()?, pr.head_sha);
    assert_eq!(pr.head_repo, "contributor/fork");
    if scenario.starts_with("prefetch-") {
        return prefetch_checkout(&client, pr, &fixture, &scenario);
    }
    if scenario.starts_with("base-tip-") {
        return base_tip_checkout(&client, pr, &fixture, &scenario);
    }
    if scenario.starts_with("pending-") {
        return pending_import_checkout(&client, &fixture, &scenario);
    }
    if scenario.starts_with("track-") {
        return tracking_checkout(&client, &fixture, &scenario);
    }
    if scenario.starts_with("ignore-") {
        return ignored_checkout(&client, pr, &fixture, &scenario);
    }
    if scenario == "empty" {
        jj(&directory, &["describe", "-m", ""])?;
    }
    let recorded = client.checkout_commit()?;
    let mut unrelated = None;
    if scenario == "dirty" {
        jj(
            &directory,
            &["new", "root()", "-m", "Unrelated remote work"],
        )?;
        let sha = client.checkout_commit()?;
        git(
            &client,
            &["update-ref", "refs/remotes/unrelated/deleted", &sha],
        )?;
        jj(&directory, &["--ignore-working-copy", "git", "import"])?;
        jj(&directory, &["edit", &recorded])?;
        git(
            &client,
            &["update-ref", "-d", "refs/remotes/unrelated/deleted"],
        )?;
        let error = format!("{:#}", client.fetch_pr(&pr).unwrap_err());
        assert!(
            error.contains("refs/remotes/unrelated/deleted")
                && error.contains("pending Git changes"),
            "{error}"
        );
        assert_eq!(client.checkout_commit()?, recorded);
        // Restore the fixture's Git ref explicitly, then continue the dirty checkout
        // case. Production never rolls back pending refs after a refusal.
        git(
            &client,
            &["update-ref", "refs/remotes/unrelated/deleted", &sha],
        )?;
        unrelated = Some(sha);
    }
    if scenario != "empty" {
        git(&client, &["update-ref", "refs/heads/user-kept", &recorded])?;
        jj(&directory, &["--ignore-working-copy", "git", "import"])?;
    }
    let hooks = fixture.join("hooks");
    fs::create_dir(&hooks)?;
    let hook = hooks.join("reference-transaction");
    support::install(
        &hook,
        &json!({"tool": "transport-hook", "marker": fixture.join("hook-ran")}),
    )?;
    git(
        &client,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    )?;
    // An unrelated checkout must not qualify for review discovery.
    assert!(Client::discover(&directory, Some("42"), Some("upstream/project")).is_err());
    fs::write(directory.join("keep.txt"), "unsnapshotted local changes\n")?;
    if scenario == "url-rewrite" {
        git(
            &client,
            &[
                "config",
                &format!(
                    "url.file://{}.insteadOf",
                    fixture.join("base.git").display()
                ),
                URL,
            ],
        )?;
    }
    if scenario == "ref-race" {
        output(
            Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(fixture.join("base.git"))
                .args(["update-ref", "refs/pull/42/head", &pr.base_sha]),
            b"",
        )?;
    }
    if scenario == "shallow" {
        output(
            Command::new(std::env::var_os("ZPR_REAL_GIT").unwrap())
                .arg("--no-pager")
                .arg("--git-dir")
                .arg(&client.git_dir)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "fetch",
                    "--depth=1",
                    "--no-write-fetch-head",
                    &format!("file://{}", fixture.join("base.git").display()),
                    "refs/pull/42/head:refs/zed-pr-review-shallow-fixture",
                ]),
            b"",
        )?;
    }
    let fetched = client.fetch_pr(&pr);
    assert_eq!(
        client.checkout_commit()?,
        recorded,
        "fetch must not snapshot @"
    );
    assert_eq!(
        fs::read_to_string(directory.join("keep.txt"))?,
        "unsnapshotted local changes\n"
    );
    assert!(
        !directory.join("file.txt").exists(),
        "fetch must not change working-copy files"
    );
    if ["api-race", "ref-race", "url-rewrite"].contains(&scenario.as_str()) {
        let error = format!("{:#}", fetched.unwrap_err());
        match scenario.as_str() {
            "api-race" => assert!(
                error.contains("base branch changed during fetch"),
                "{error}"
            ),
            "ref-race" => assert!(
                error.contains("head") && error.contains("moved during fetch"),
                "{error}"
            ),
            "url-rewrite" => assert!(error.contains("URL rewriting"), "{error}"),
            _ => unreachable!(),
        }
        return Ok(());
    }

    fetched?;
    assert!(
        !fixture.join("hook-ran").exists(),
        "fetch/import must not run hooks"
    );
    if scenario != "empty" {
        assert_eq!(
            git(&client, &["rev-parse", "refs/heads/user-kept"])?,
            recorded
        );
    }
    if let Some(sha) = unrelated {
        assert_eq!(
            jj(
                &directory,
                &[
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    &format!("all() & {sha}"),
                    "--no-graph",
                    "-T",
                    "commit_id"
                ]
            )?,
            sha,
            "import must not abandon unrelated remote work"
        );
    }
    assert_eq!(client.merge_base(&pr)?, std::env::var("ZPR_COMMON")?);
    assert_ne!(
        client.merge_base(&pr)?,
        pr.base_sha,
        "base branch diverged from the fork"
    );
    assert_eq!(client.blob(&pr.head_sha, "file.txt")?, b"fork head\n");
    assert_eq!(
        jj(
            &directory,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                &pr.head_sha,
                "--no-graph",
                "-T",
                "commit_id"
            ]
        )?,
        pr.head_sha
    );
    let refs = git(
        &client,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/zed-pr-review",
        ],
    )?;
    assert_eq!(refs.lines().count(), 2);
    // A second fetch uses new refs, and never rewrites or deletes the first pair.
    client.fetch_pr(&pr)?;
    let refs_again = git(
        &client,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/zed-pr-review",
        ],
    )?;
    assert_eq!(refs_again.lines().count(), 4);
    for reference in refs.lines() {
        assert!(refs_again.lines().any(|r| r == reference));
    }
    assert!(!client.git_dir.join("FETCH_HEAD").exists());
    if scenario == "empty" {
        fs::remove_file(directory.join("keep.txt"))?;
    }
    if scenario == "obstruction" {
        fs::write(directory.join(".gitignore"), "file.txt\n")?;
        fs::write(directory.join("file.txt"), "ignored obstruction\n")?;
    }
    if scenario == "directory-obstruction" {
        fs::write(directory.join(".gitignore"), "file.txt/\n")?;
        fs::create_dir(directory.join("file.txt"))?;
        fs::write(directory.join("file.txt/private"), "keep this\n")?;
    }
    if scenario.starts_with("immutable") {
        fs::write(
            fixture.join("jj.toml"),
            format!(
                "[user]\nname='Test'\nemail='test@example.test'\n[revset-aliases]\n'immutable_heads()' = '{}'\n",
                pr.head_sha
            ),
        )?;
    }
    if scenario == "conflicts" {
        jj(&directory, &["new", &pr.head_sha, &pr.base_sha])?;
    }
    if scenario == "stale" {
        jj(&directory, &["--ignore-working-copy", "new", &pr.base_sha])?;
    }
    let mut checkout_pr = pr.clone();
    if scenario == "target-conflicts" {
        jj(&directory, &["new", &pr.head_sha, &pr.base_sha])?;
        checkout_pr.head_sha = client.checkout_commit()?;
        jj(&directory, &["edit", &recorded])?;
    }
    if scenario == "sparse" {
        jj(&directory, &["sparse", "set", "--clear"])?;
    }
    let before = client.checkout_commit()?;
    let target_before = git(&client, &["cat-file", "commit", &pr.head_sha])?;
    if scenario == "immutable-late-edit" {
        let real_jj = PathBuf::from(std::env::var_os("ZPR_REAL_JJ").unwrap());
        support::install(
            &fixture.join("bin/jj"),
            &json!({
                "tool": "transport-jj", "real_jj": real_jj,
                "after_success": {
                    "commands": ["new"],
                    "path": "file.txt", "contents": "late tracked edit\n",
                },
            }),
        )?;
    }
    let bookmark_names = || -> Result<std::collections::BTreeSet<String>> {
        Ok(jj(
            &directory,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "-T",
                "name ++ \"\\n\"",
            ],
        )?
        .lines()
        .map(str::to_owned)
        .collect())
    };
    let bookmarks_before = bookmark_names()?;
    let result = client.checkout_pr(&checkout_pr);
    assert_eq!(
        bookmark_names()?,
        bookmarks_before,
        "Checkout must not create recovery bookmarks, even on failure"
    );
    assert_eq!(
        git(&client, &["cat-file", "commit", &pr.head_sha])?,
        target_before
    );
    if scenario == "immutable-late-edit" {
        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains("Previous work was snapshotted as"),
            "{error}"
        );
        let previous = error
            .split("Return with: jj edit ")
            .nth(1)
            .and_then(|text| text.split_whitespace().next())
            .context("Missing recovery SHA")?;
        assert_eq!(
            client.blob(previous, "keep.txt")?,
            b"unsnapshotted local changes\n"
        );
        assert!(!client.checkout_matches(&pr)?);
        assert!(client.ensure_checkout(&pr).is_err());
        assert_eq!(
            fs::read_to_string(directory.join("file.txt"))?,
            "late tracked edit\n"
        );
        return Ok(());
    }
    if [
        "obstruction",
        "directory-obstruction",
        "target-conflicts",
        "sparse",
        "conflicts",
        "stale",
    ]
    .contains(&scenario.as_str())
    {
        assert!(result.is_err(), "must refuse {scenario}");
        assert_ne!(client.checkout_commit()?, pr.head_sha);
        if !["obstruction", "directory-obstruction"].contains(&scenario.as_str()) {
            assert_eq!(client.checkout_commit()?, before);
        }
        if scenario == "obstruction" {
            assert_eq!(
                fs::read_to_string(directory.join("file.txt"))?,
                "ignored obstruction\n"
            );
        }
        return Ok(());
    }
    let previous = result?;
    assert_review_checkout(&client, &pr.head_sha, scenario == "immutable")?;
    client.ensure_checkout(&pr)?;
    assert_eq!(
        fs::read_to_string(directory.join("file.txt"))?,
        "fork head\n"
    );
    assert_eq!(
        jj(
            &directory,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                &previous,
                "--no-graph",
                "-T",
                "commit_id"
            ]
        )?,
        previous,
        "The recovery SHA must resolve without a bookmark"
    );
    if scenario != "empty" {
        assert_ne!(previous, recorded);
        assert_eq!(
            client.blob(&previous, "keep.txt")?,
            b"unsnapshotted local changes\n"
        );
    } else {
        assert_eq!(previous, recorded, "empty prior work must stay reachable");
    }
    let resolved = Client::discover(&directory, Some("42"), Some("upstream/project"))?;
    assert_eq!(resolved.1.head_sha, pr.head_sha);
    // Exercise the printed return command, including an empty, undescribed
    // revision that jj can abandon when moving away without a bookmark.
    jj(&directory, &["edit", &previous])?;
    assert_eq!(client.checkout_commit()?, previous);
    if scenario != "empty" {
        assert_eq!(
            fs::read(directory.join("keep.txt"))?,
            b"unsnapshotted local changes\n"
        );
    }
    let calls = fs::read_to_string(fixture.join("git-calls.jsonl"))?;
    for call in calls.lines() {
        let args: Vec<String> = serde_json::from_str(call)?;
        assert!(
            !args
                .iter()
                .any(|a| ["checkout", "reset", "rebase", "pull"].contains(&a.as_str()))
        );
    }
    let jj_calls = fs::read_to_string(fixture.join("jj-calls.jsonl"))?;
    for call in jj_calls.lines() {
        let args: Vec<String> = serde_json::from_str(call)?;
        assert!(!args.iter().any(|a| a == "--ignore-immutable"), "{args:?}");
    }
    let gh_calls = fs::read_to_string(fixture.join("gh-calls.jsonl"))?;
    for call in gh_calls.lines() {
        let args: Value = serde_json::from_str(call)?;
        assert!(
            !args
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "POST" || a == "pr")
        );
    }
    Ok(())
}
