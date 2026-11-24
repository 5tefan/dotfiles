#![cfg(unix)]
//! Local-only transport tests. The child process has isolated HOME/JJ_CONFIG/PATH.
//! Its Git shim substitutes local bare repositories for the two allowed HTTPS URLs;
//! the real Git still performs every fetch, ref update, and object import.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
        for (reference, sha) in [
            ("refs/heads/main", &f.base),
            ("refs/heads/feature/topic", &f.base),
            ("refs/pull/42/head", &f.head),
        ] {
            f.git(&["update-ref", reference, sha])?;
        }
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
        let git_shim = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
root = os.environ['ZPR_FIXTURE']
with open(root + '/git-calls.jsonl', 'a') as f: f.write(json.dumps(args) + '\n')
def inject_pending(stage):
    path = root + '/pending-ref.json'
    if not os.path.exists(path): return
    change = json.load(open(path))
    if change['stage'] != stage: return
    update = ['update-ref', change['reference'], change['target']] if change['target'] else ['update-ref', '-d', change['reference']]
    subprocess.run([os.environ['ZPR_REAL_GIT'], '--no-pager', '--git-dir', change['git_dir'], '-c', 'core.hooksPath=/dev/null'] + update, check=True)
    os.rename(path, root + '/injected-ref.json')
if 'fetch' in args:
    urls = {'https://github.com/upstream/project.git': '/base.git',
            'https://github.com/contributor/fork.git': '/fork.git'}
    requested = [a for a in args if a in urls]
    assert len(requested) == 1, args
    expected = requested[0]
    refspecs = args[args.index('--')+2:]
    if any(r.startswith('refs/pull/') for r in refspecs):
        assert expected == 'https://github.com/upstream/project.git'
        assert len(refspecs) == 2, args
        assert refspecs[0].startswith('refs/pull/42/head:refs/heads/zed-pr-review/'), args
        assert refspecs[1].startswith('refs/heads/main:refs/heads/zed-pr-review/'), args
    else:
        assert len(refspecs) == 1, args
        branch = json.load(open(root + '/control.json'))['head']['ref']
        assert refspecs[0].startswith('refs/heads/' + branch + ':refs/heads/zed-pr-review/'), args
    assert not any(a.startswith('+') for a in args), args
    assert os.environ['GIT_ALLOW_PROTOCOL'] == 'https'
    for flag in ['--atomic', '--no-tags', '--no-prune', '--no-prune-tags', '--refmap=',
                 '--no-write-fetch-head', '--no-recurse-submodules', '--no-auto-maintenance',
                 '--no-write-commit-graph', '--no-filter']:
        assert flag in args, args
    assert 'http.followRedirects=false' in args and 'core.hooksPath=/dev/null' in args
    args[args.index(expected)] = root + urls[expected]
    os.environ['GIT_ALLOW_PROTOCOL'] = 'file'
    result = subprocess.run([os.environ['ZPR_REAL_GIT']] + args)
    inject_pending('first')
    if os.environ.get('ZPR_SCENARIO') == 'api-race':
        p = root + '/control.json'
        data = json.load(open(p)); data['base']['ref'] = 'moved'; json.dump(data, open(p, 'w'))
    sys.exit(result.returncode)
if 'update-ref' in args and 'refs/remotes/origin/feature/topic' in args:
    result = subprocess.run([os.environ['ZPR_REAL_GIT']] + args)
    inject_pending('second')
    sys.exit(result.returncode)
if 'ls-remote' in args: assert '--get-url' in args, args
assert not any(a in ['push', 'pull', 'clone'] for a in args), args
os.execv(os.environ['ZPR_REAL_GIT'], [os.environ['ZPR_REAL_GIT']] + args)
"#;
        let gh_shim = r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
root = os.environ['ZPR_FIXTURE']
with open(root + '/gh-calls.jsonl', 'a') as f: f.write(json.dumps(args) + '\n')
if args == ['repo', 'view', '--json', 'nameWithOwner,url']:
    assert os.path.isdir(os.environ['GIT_DIR'])
    print(json.dumps({'nameWithOwner':'upstream/project','url':'https://github.com/upstream/project'}))
elif args[0] == 'api' and '--method' in args and args[args.index('--method')+1] == 'GET':
    if args[-1] == 'user': print(json.dumps({'login':'reviewer'}))
    elif args[-1] == 'repos/upstream/project/pulls/42': print(open(root + '/control.json').read())
    else: raise Exception(args)
else: raise Exception(args)
"#;
        let jj_shim = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
root = os.environ['ZPR_FIXTURE']
with open(root + '/jj-calls.jsonl', 'a') as f: f.write(json.dumps(args) + '\n')
result = subprocess.run([os.environ['ZPR_REAL_JJ']] + args, stderr=subprocess.PIPE)
sys.stderr.buffer.write(result.stderr)
if result.returncode:
    with open(root + '/jj-errors.log', 'ab') as f: f.write(result.stderr)
sys.exit(result.returncode)
"#;
        for (name, contents) in [("git", git_shim), ("gh", gh_shim), ("jj", jj_shim)] {
            let p = f.root.join("bin").join(name);
            fs::write(&p, contents)?;
            fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
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

#[test]
fn local_transport_colocated_and_noncolocated() -> Result<()> {
    for colocate in [false, true] {
        for scenario in [
            "dirty",
            "empty",
            "obstruction",
            "directory-obstruction",
            "target-conflicts",
            "sparse",
            "shallow",
            "conflicts",
            "stale",
            "api-race",
            "ref-race",
            "url-rewrite",
        ] {
            let f = Fixture::new(colocate)?;
            f.run(scenario)
                .with_context(|| format!("colocate={colocate}, scenario={scenario}"))?;
        }
    }
    Ok(())
}

fn base_tip_scenarios(scenarios: &[&str], shallow: bool) -> Result<()> {
    for colocate in [false, true] {
        for scenario in scenarios {
            let f = Fixture::new(colocate)?;
            let actual = match *scenario {
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
            if *scenario == "unavailable" {
                // A real commit exists on the server, but no fetched ref reaches it.
                let missing = f.commit("unavailable pinned base\n", Some(&f.base))?;
                let mut control: Value =
                    serde_json::from_slice(&fs::read(f.root.join("control.json"))?)?;
                control["base"]["sha"] = json!(missing);
                fs::write(f.root.join("control.json"), serde_json::to_vec(&control)?)?;
            }
            let child_scenario = if ["api-race", "ref-race"].contains(scenario)
                || scenario.starts_with("pending-")
            {
                assert!(!shallow);
                (*scenario).to_owned()
            } else {
                format!(
                    "base-tip-{scenario}{}",
                    if shallow { "-shallow" } else { "" }
                )
            };
            f.run(&child_scenario).with_context(|| {
                format!("colocate={colocate}, shallow={shallow}, scenario={scenario}")
            })?;
        }
    }
    Ok(())
}

#[test]
fn base_tip_fast_forward_colocated_and_noncolocated() -> Result<()> {
    base_tip_scenarios(&["fast-forward"], false)
}

#[test]
fn base_tip_fast_forward_shallow_colocated_and_noncolocated() -> Result<()> {
    base_tip_scenarios(&["fast-forward"], true)
}

#[test]
fn base_tip_refusals_colocated_and_noncolocated() -> Result<()> {
    base_tip_scenarios(&["rewind", "divergent", "unavailable"], false)
}

#[test]
fn base_tip_refusals_shallow_colocated_and_noncolocated() -> Result<()> {
    base_tip_scenarios(&["rewind", "divergent", "unavailable"], true)
}

#[test]
fn base_tip_pending_private_ref_colocated_and_noncolocated() -> Result<()> {
    for shallow in [false, true] {
        base_tip_scenarios(&["unrelated-ref"], shallow)?;
    }
    Ok(())
}

#[test]
fn base_tip_existing_guards_colocated_and_noncolocated() -> Result<()> {
    base_tip_scenarios(
        &[
            "api-race",
            "ref-race",
            "pending-before-fallback-update",
            "pending-first-fallback-update",
        ],
        false,
    )
}

#[test]
fn immutable_heads_colocated_and_noncolocated() -> Result<()> {
    tracking_scenarios(&[
        "immutable",
        "immutable-late-edit",
        "ignore-removed-immutable",
        "ignore-kept-immutable",
        "ignore-late-file-immutable",
    ])
}

#[test]
fn ignored_files_colocated_and_noncolocated() -> Result<()> {
    for colocate in [false, true] {
        for scenario in [
            "ignore-removed",
            "ignore-kept",
            "ignore-negated",
            "ignore-node-modules",
            "ignore-node-modules-contents",
            "ignore-nested-removed",
            "ignore-nested-kept",
            "ignore-excludes",
            "ignore-relative-excludes",
            "ignore-core-changed",
            "ignore-late-file",
        ] {
            let f = Fixture::new(colocate)?;
            f.run(scenario)
                .with_context(|| format!("colocate={colocate}, scenario={scenario}"))?;
        }
    }
    Ok(())
}

fn tracking_scenarios(scenarios: &[&str]) -> Result<()> {
    for colocate in [false, true] {
        for scenario in scenarios {
            let f = Fixture::new(colocate)?;
            f.run(scenario)
                .with_context(|| format!("colocate={colocate}, scenario={scenario}"))?;
        }
    }
    Ok(())
}

// Keep the groups bounded: each creates independent real Git/jj repositories.
#[test]
fn source_tracking_creation() -> Result<()> {
    tracking_scenarios(&[
        "track-same-repo",
        "track-fork-present",
        "track-fork-absent",
        "track-wrong-host",
        "track-local-equal",
        "track-literal-name",
    ])
}

#[test]
fn source_tracking_updates() -> Result<()> {
    tracking_scenarios(&[
        "track-up-to-date",
        "track-at-head-dirty",
        "track-fast-forward",
        "track-local-divergent",
        "track-tracked-divergent",
        "track-deleted",
        "track-remote-rewind",
    ])
}

#[test]
fn source_tracking_refusals() -> Result<()> {
    tracking_scenarios(&[
        "track-source-mismatch",
        "track-source-rewrite",
        "track-ambiguous",
        "track-pending-git",
        "track-pending-remote",
        "track-local-conflict",
    ])
}

#[test]
fn source_tracking_guards() -> Result<()> {
    tracking_scenarios(&[
        "track-auto",
        "track-auto-conflict",
        "track-other-remote",
        "track-immutable",
        "track-ignored",
    ])
}

#[test]
fn source_tracking_pending_refs() -> Result<()> {
    tracking_scenarios(&[
        "track-pending-delete",
        "track-multiple-urls",
        "track-symbolic-remote",
    ])
}

#[test]
fn pending_import_source_refs() -> Result<()> {
    tracking_scenarios(&[
        "pending-before-source-delete",
        "pending-before-source-rewind",
        "pending-before-source-update",
    ])
}

#[test]
fn pending_import_fallback_refs() -> Result<()> {
    tracking_scenarios(&[
        "pending-before-fallback-delete",
        "pending-before-fallback-rewind",
        "pending-before-fallback-update",
    ])
}

#[test]
fn pending_import_after_fetch() -> Result<()> {
    tracking_scenarios(&[
        "pending-first-source-delete",
        "pending-first-source-rewind",
        "pending-first-source-update",
        "pending-first-fallback-delete",
        "pending-first-fallback-rewind",
        "pending-first-fallback-update",
    ])
}

#[test]
fn pending_import_after_source_publication() -> Result<()> {
    tracking_scenarios(&[
        "pending-second-source-delete",
        "pending-second-source-rewind",
        "pending-second-source-update",
    ])
}

#[test]
fn pending_import_local_refs() -> Result<()> {
    tracking_scenarios(&[
        "pending-before-local-delete",
        "pending-before-local-rewind",
        "pending-before-local-update",
        "pending-before-tag-delete",
        "pending-before-tag-rewind",
        "pending-before-tag-update",
    ])
}

#[test]
fn pending_import_new_refs() -> Result<()> {
    tracking_scenarios(&[
        "pending-before-newlocal-update",
        "pending-before-newremote-update",
        "pending-before-newtag-update",
    ])
}

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
    let target_rules = match scenario {
        "ignore-kept" | "ignore-late-file" => ".env\n",
        "ignore-negated" => ".env\n!.env\n",
        "ignore-node-modules" => ".env\nnode_modules/\n",
        "ignore-node-modules-contents" => ".env\nnode_modules/*\n",
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
        if nested || scenario.ends_with("excludes") || scenario == "ignore-core-changed" {
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
        fs::write(
            &wrapper,
            format!(
                "#!/usr/bin/env python3\nimport subprocess, sys\nr = subprocess.run([{}] + sys.argv[1:])\nif r.returncode == 0 and any(a in sys.argv[1:] for a in ['edit', 'new']):\n    open('.late-private', 'w').write('late private data\\n')\nsys.exit(r.returncode)\n",
                serde_json::to_string(&real_jj)?
            ),
        )?;
        fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700))?;
        private_files.push((".late-private".into(), "late private data\n".into()));
    }
    let rejected = matches!(
        scenario,
        "ignore-removed" | "ignore-negated" | "ignore-nested-removed" | "ignore-core-changed"
    );
    let result = client.checkout_pr(&pr);
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
    fs::write(
        &hook,
        "#!/usr/bin/env python3\nimport os\nopen(os.environ['ZPR_FIXTURE'] + '/tracking-hook-ran', 'w').write('unsafe')\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
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
    fs::write(
        &hook,
        "#!/usr/bin/env python3\nimport os\nopen(os.environ['ZPR_FIXTURE'] + '/hook-ran', 'w').write('unsafe')\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
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
        fs::write(
            fixture.join("bin/jj"),
            format!(
                "#!/usr/bin/env python3\nimport subprocess, sys\nr = subprocess.run([{}] + sys.argv[1:])\nif r.returncode == 0 and 'new' in sys.argv[1:]:\n    open('file.txt', 'w').write('late tracked edit\\n')\nsys.exit(r.returncode)\n",
                serde_json::to_string(&real_jj)?
            ),
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
