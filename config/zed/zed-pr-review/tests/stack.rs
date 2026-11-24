#![cfg(unix)]
//! Stack tests use only scripted REST reads in isolated child processes. No live gh,
//! Git, jj, GraphQL, checkout, commit ancestry, or mutation commands are permitted.

use anyhow::{Context, Result, ensure};
use ratatui::style::Modifier;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use zed_pr_review::github::Client;
use zed_pr_review::model::PullRequest;
use zed_pr_review::stack::{RenderedStack, Stack};

const REPO: &str = "acme/widgets";
const HOST: &str = "github.com";

fn oid(n: u64) -> String {
    format!("{n:040x}")
}

fn pr(number: u64, head: &str, base: &str, head_sha: u64, base_sha: u64) -> Value {
    json!({
        "number": number, "html_url": format!("https://{HOST}/{REPO}/pull/{number}"),
        "title": format!("Change {number}"), "state": "open", "draft": false,
        "merged_at": null, "user": {"login": "author"},
        "head": {"sha": oid(head_sha), "ref": head, "repo": {"full_name": REPO}},
        "base": {"sha": oid(base_sha), "ref": base, "repo": {"full_name": REPO}}
    })
}

fn pin(value: &Value, host: &str) -> PullRequest {
    PullRequest {
        host: host.into(),
        repo: REPO.into(),
        number: value["number"].as_u64().unwrap(),
        url: value["html_url"].as_str().unwrap().into(),
        title: value["title"].as_str().unwrap().into(),
        head_sha: value["head"]["sha"].as_str().unwrap().into(),
        base_sha: value["base"]["sha"].as_str().unwrap().into(),
        head_repo: value["head"]["repo"]["full_name"].as_str().unwrap().into(),
        head_ref: value["head"]["ref"].as_str().unwrap().into(),
        author: "author".into(),
        reviewer: "reviewer".into(),
    }
}

fn read(value: &Value) -> Value {
    json!({"endpoint": format!("repos/{REPO}/pulls/{}", value["number"]), "body": value})
}

fn user() -> Value {
    json!({"endpoint": "user", "body": {"login": "reviewer"}})
}

fn discovery(current: &Value, pages: Vec<Vec<Value>>, mut connected: Vec<Value>) -> Vec<Value> {
    let mut steps = vec![
        user(),
        read(current),
        json!({"endpoint": format!("repos/{REPO}/pulls?state=open&per_page=100"), "pages": true, "body": pages}),
    ];
    connected.sort_by_key(|p| p["number"].as_u64().unwrap());
    steps.extend(connected.iter().map(read));
    steps.push(user());
    steps
}

fn plan(current: &Value, pages: Vec<Vec<Value>>, connected: Vec<Value>) -> Value {
    json!({"host": HOST, "current": pin(current, HOST), "steps": discovery(current, pages, connected)})
}

fn run(plan: Value) -> Result<Value> {
    let temp = tempfile::Builder::new().prefix("zpr-stack-").tempdir()?;
    let root = temp.path().canonicalize()?;
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    fs::create_dir(root.join("home"))?;
    fs::write(root.join("plan.json"), serde_json::to_vec(&plan)?)?;
    fs::write(bin.join("gh"), include_str!("stack_gh.py"))?;
    for name in ["git", "jj"] {
        fs::write(
            bin.join(name),
            "#!/bin/sh\necho 'unexpected Git/jj command' >&2\nexit 99\n",
        )?;
    }
    for name in ["gh", "git", "jj"] {
        fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o700))?;
    }
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").context("Missing PATH")?,
    ));
    let output = Command::new(std::env::current_exe()?)
        .args(["--exact", "stack_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::join_paths(paths)?)
        .env("HOME", root.join("home"))
        .env("GH_HOST", "wrong.example.test")
        .env("GH_REPO", "wrong/repository")
        .env("ZPR_STACK_FIXTURE", &root)
        .current_dir(&root)
        .output()?;
    ensure!(
        output.status.success(),
        "Stack child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&fs::read(root.join("result.json"))?)?;
    if result.get("error").is_none() {
        let count = fs::read_to_string(root.join("index"))?.parse::<usize>()?;
        ensure!(
            count == plan["steps"].as_array().unwrap().len(),
            "Not all expected reads were performed: {count}"
        );
    }
    Ok(result)
}

fn success(plan: Value) -> Result<Value> {
    let result = run(plan)?;
    ensure!(result.get("error").is_none(), "Unexpected error: {result}");
    let plain = result["render"].as_str().unwrap();
    let lines = rendered_lines(&result);
    assert_eq!(plain, format!("{}\n", lines.join("\n")));
    assert!(!plain.contains('\x1b'));
    let colored = result["colored"].as_str().unwrap();
    assert_eq!(without_sgr(colored), plain);
    assert!(colored.ends_with("\x1b[0m\n"));
    let current_row = result["current_row"].as_u64().unwrap() as usize;
    assert!(lines[current_row].contains(&format!("@ #{} ", result["current"])));
    assert_eq!(
        result["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|line| {
                let spans = line["spans"].as_array().unwrap();
                spans.len() >= 8 && spans[1]["text"] == "@"
            })
            .count(),
        1
    );
    Ok(result)
}

fn rendered_lines(result: &Value) -> Vec<&str> {
    result["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["text"].as_str().unwrap())
        .collect()
}

// Accept only SGR escapes, so a matching plain rendering cannot hide an OSC
// hyperlink, cursor movement, or another terminal command in colored output.
fn without_sgr(colored: &str) -> String {
    let mut plain = String::new();
    let mut chars = colored.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
            continue;
        }
        assert_eq!(chars.next(), Some('['));
        loop {
            let code = chars.next().expect("unterminated SGR");
            if code == 'm' {
                break;
            }
            assert!(code.is_ascii_digit() || code == ';', "non-SGR escape");
        }
    }
    plain
}

fn graph(result: &Value) -> Vec<String> {
    rendered_lines(result)
        .into_iter()
        .filter_map(|line| {
            if let Some((prefix, metadata)) = line.split_once(" #") {
                Some(format!(
                    "{prefix} #{}",
                    metadata.split_whitespace().next().unwrap()
                ))
            } else if line.ends_with('╯') {
                Some(line.to_owned())
            } else {
                None
            }
        })
        .collect()
}

fn rejected(plan: Value, message: &str) -> Result<()> {
    let result = run(plan)?;
    let error = result["error"]
        .as_str()
        .context("Expected discovery to fail")?;
    ensure!(
        error.contains(message),
        "Expected {message:?}, got {error:?}"
    );
    Ok(())
}

#[test]
fn stack_child() -> Result<()> {
    let Some(root) = std::env::var_os("ZPR_STACK_FIXTURE") else {
        return Ok(());
    };
    let root = std::path::PathBuf::from(root);
    let plan: Value = serde_json::from_slice(&fs::read(root.join("plan.json"))?)?;
    let current: PullRequest = serde_json::from_value(plan["current"].clone())?;
    let client = Client {
        root: root.clone(),
        git_dir: root.clone(),
        host: plan["host"].as_str().unwrap().into(),
        repo: REPO.into(),
        reviewer: "reviewer".into(),
    };
    let result = (|| -> Result<Value> {
        let stack = Stack::discover(&client, &current)?;
        if let Some(selected) = plan.get("selected") {
            let number = selected.as_u64().unwrap();
            let refreshed = client.read_pr(number)?;
            stack.verify_adjacent(&client, &refreshed)?;
        }
        let rendered: RenderedStack = stack.render_log();
        let lines: Vec<_> = rendered
            .lines
            .iter()
            .map(|line| {
                let spans: Vec<_> = line
                    .spans
                    .iter()
                    .map(|span| {
                        let style = line.style.patch(span.style);
                        json!({
                            "text": span.content,
                            "fg": style.fg.map(|color| format!("{color:?}")),
                            "bold": style.add_modifier.contains(Modifier::BOLD),
                            "dim": style.add_modifier.contains(Modifier::DIM),
                        })
                    })
                    .collect();
                json!({"text": line.to_string(), "spans": spans})
            })
            .collect();
        Ok(json!({
            "render": stack.render(), "colored": stack.render_colored(),
            "lines": lines, "current_row": rendered.current_row,
            "current": stack.current().pr.number,
            "parent": stack.parent()?.map(|entry| entry.pr.number),
            "children": stack.children().iter().map(|entry| entry.pr.number).collect::<Vec<_>>()
        }))
    })();
    let result = match result {
        Ok(value) => value,
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    fs::write(root.join("result.json"), serde_json::to_vec(&result)?)?;
    Ok(())
}

#[test]
fn linear_stack_uses_branches_not_numbers_titles_or_commit_counts() -> Result<()> {
    let bottom = pr(90, "arbitrary-foundation", "main", 10, 1);
    let mut middle = pr(7, "unrelated-name", "arbitrary-foundation", 30, 10);
    middle["commits"] = 12.into();
    middle["draft"] = true.into();
    let mut top = pr(3, "another-name", "unrelated-name", 50, 30);
    top["body"] = "Depends on #999, not #7".into();
    let all = vec![top.clone(), bottom.clone(), middle.clone()];
    let result = success(plan(&middle, vec![all.clone()], all))?;
    assert_eq!(result["parent"], 90);
    assert_eq!(result["children"], json!([3]));
    let text = result["render"].as_str().unwrap();
    assert_eq!(
        text,
        concat!(
            "○ #3 another-name [ready]\n",
            "│ Change 3\n",
            "│ https://github.com/acme/widgets/pull/3\n",
            "@ #7 unrelated-name [draft] (current)\n",
            "│ Change 7\n",
            "│ https://github.com/acme/widgets/pull/7\n",
            "○ #90 arbitrary-foundation [ready]\n",
            "│ Change 90\n",
            "│ https://github.com/acme/widgets/pull/90\n",
            "◆ main (base)\n",
        )
    );
    assert_eq!(result["current_row"], 3);
    assert!(!text.contains(" -> "));
    assert!(!text.contains("acme/widgets:"));
    Ok(())
}

#[test]
fn branch_points_return_every_child_and_include_the_connected_tree() -> Result<()> {
    let bottom = pr(1, "topic", "main", 10, 1);
    let left = pr(4, "left", "topic", 20, 10);
    let right = pr(2, "right", "topic", 30, 10);
    let top = pr(8, "top", "left", 40, 20);
    let all = vec![top, left.clone(), bottom.clone(), right];
    let result = success(plan(&bottom, vec![all.clone()], all.clone()))?;
    assert_eq!(result["parent"], Value::Null);
    assert_eq!(result["children"], json!([2, 4]));
    assert_eq!(graph(&result), ["○ #2", "│ ○ #8", "│ ○ #4", "├─╯", "@ #1"]);
    assert_eq!(result["current_row"], 10);
    let result = success(plan(&left, vec![all.clone()], all))?;
    assert_eq!(result["parent"], 1);
    assert_eq!(result["children"], json!([8]));
    assert_eq!(graph(&result), ["○ #2", "│ ○ #8", "│ @ #4", "├─╯", "○ #1"]);
    assert_eq!(result["current_row"], 6);
    let lines = rendered_lines(&result);
    assert_eq!(lines[4], "│ │ Change 8");
    assert_eq!(lines[7], "│ │ Change 4");
    assert_eq!(lines[9], "├─╯");
    assert_eq!(lines.last(), Some(&"◆ main (base)"));
    Ok(())
}

#[test]
fn nested_forks_keep_edges_separate_and_ignore_page_order() -> Result<()> {
    let all = vec![
        pr(1, "one", "main", 10, 1),
        pr(2, "two", "one", 20, 10),
        pr(3, "three", "one", 30, 10),
        pr(4, "four", "one", 40, 10),
        pr(5, "five", "two", 50, 20),
        pr(6, "six", "two", 60, 20),
        pr(7, "seven", "three", 70, 30),
        pr(8, "eight", "three", 80, 30),
        pr(9, "nine", "six", 90, 60),
        pr(10, "ten", "six", 100, 60),
    ];
    let current = &all[5];
    let result = success(plan(current, vec![all.clone()], all.clone()))?;
    assert_eq!(
        graph(&result),
        [
            "○ #5",
            "│ ○ #9",
            "│ │ ○ #10",
            "│ ├─╯",
            "│ @ #6",
            "├─╯",
            "○ #2",
            "│ ○ #7",
            "│ │ ○ #8",
            "│ ├─╯",
            "│ ○ #3",
            "├─╯",
            "│ ○ #4",
            "├─╯",
            "○ #1",
        ]
    );
    assert_eq!(result["current_row"], 10);
    let lines = rendered_lines(&result);
    assert_eq!(lines[7], "│ │ │ Change 10");
    assert_eq!(lines[11], "│ │ Change 6");
    assert_eq!(lines[15], "│ Change 2");
    let mut shuffled = all.clone();
    shuffled.reverse();
    shuffled.rotate_left(3);
    let reordered = success(plan(
        current,
        vec![shuffled[..4].to_vec(), vec![], shuffled[4..].to_vec()],
        all.clone(),
    ))?;
    for key in ["render", "colored", "lines", "current_row"] {
        assert_eq!(result[key], reordered[key], "page order changed {key}");
    }
    Ok(())
}

#[test]
fn wide_forks_reuse_lanes_after_each_join() -> Result<()> {
    let root = pr(100, "root", "main", 1000, 1);
    let mut all = vec![root.clone()];
    all.extend((1..=8).map(|n| pr(n, &format!("side-{n}"), "root", n + 10, 1000)));
    let result = success(plan(&root, vec![all.clone()], all))?;
    let mut expected = vec!["○ #1".to_owned()];
    for number in 2..=8 {
        expected.push(format!("│ ○ #{number}"));
        expected.push("├─╯".to_owned());
    }
    expected.push("@ #100".to_owned());
    assert_eq!(graph(&result), expected);
    assert_eq!(result["current_row"], 31);
    assert!(
        rendered_lines(&result)
            .iter()
            .all(|line| !line.starts_with("│ │ │"))
    );
    Ok(())
}

#[test]
fn deep_linear_stacks_never_increase_the_graph_indent() -> Result<()> {
    let depth = 128;
    let all: Vec<_> = (1..=depth)
        .map(|n| {
            let base = if n == 1 {
                "main".into()
            } else {
                format!("topic-{}", n - 1)
            };
            pr(n, &format!("topic-{n}"), &base, n + 1, n)
        })
        .collect();
    let current_number = depth / 2;
    let result = success(plan(
        &all[current_number as usize - 1],
        vec![all.clone()],
        all.clone(),
    ))?;
    let lines = rendered_lines(&result);
    assert_eq!(lines.len(), 3 * depth as usize + 1);
    for (index, number) in (1..=depth).rev().enumerate() {
        let row = index * 3;
        let marker = if number == current_number { '@' } else { '○' };
        assert!(lines[row].starts_with(&format!("{marker} #{number} ")));
        assert_eq!(lines[row + 1], format!("│ Change {number}"));
        assert_eq!(
            lines[row + 2],
            format!("│ https://{HOST}/{REPO}/pull/{number}")
        );
    }
    assert_eq!(result["current_row"], 3 * (depth - current_number));
    assert_eq!(lines.last(), Some(&"◆ main (base)"));
    Ok(())
}

#[test]
fn current_row_tracks_root_middle_tip_and_single_pr() -> Result<()> {
    let all = vec![
        pr(9, "bottom", "main", 10, 1),
        pr(2, "middle", "bottom", 20, 10),
        pr(5, "tip", "middle", 30, 20),
    ];
    for (index, current) in all.iter().enumerate() {
        let result = success(plan(current, vec![all.clone()], all.clone()))?;
        assert_eq!(result["current_row"], 3 * (2 - index));
        let expected: Vec<_> = [5, 2, 9]
            .into_iter()
            .map(|number| {
                let marker = if current["number"] == number {
                    '@'
                } else {
                    '○'
                };
                format!("{marker} #{number}")
            })
            .collect();
        assert_eq!(graph(&result), expected);
    }
    let mut single = pr(42, "only", "trunk", 10, 1);
    // Text that resembles a marker must not determine the current row.
    single["title"] = "Discuss @ #42 and ○ #5".into();
    let result = success(plan(
        &single,
        vec![vec![single.clone()]],
        vec![single.clone()],
    ))?;
    assert_eq!(result["current_row"], 0);
    assert_eq!(rendered_lines(&result).len(), 4);
    assert_eq!(rendered_lines(&result)[3], "◆ trunk (base)");
    Ok(())
}

fn span_with_text<'a>(result: &'a Value, text: &str) -> &'a Value {
    result["lines"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|line| line["spans"].as_array().unwrap())
        .find(|span| span["text"] == text)
        .unwrap_or_else(|| panic!("missing span {text:?}"))
}

#[test]
fn semantic_colors_match_ansi_and_do_not_leak_into_plain_text() -> Result<()> {
    let root = pr(10, "foundation", "main", 10, 1);
    let mut current = pr(20, "next", "foundation", 20, 10);
    current["draft"] = true.into();
    let sibling = pr(30, "side", "foundation", 30, 10);
    let all = vec![root, current.clone(), sibling];
    let result = success(plan(&current, vec![all.clone()], all))?;
    for (text, color) in [
        ("#20", "Magenta"),
        ("#10", "Magenta"),
        ("next", "Cyan"),
        ("foundation", "Cyan"),
        ("main", "Cyan"),
        ("[draft]", "Yellow"),
        ("[ready]", "Green"),
        ("@", "Green"),
        (" (current)", "Green"),
    ] {
        assert_eq!(span_with_text(&result, text)["fg"], color, "{text}");
    }
    for text in ["@", " (current)"] {
        assert_eq!(span_with_text(&result, text)["bold"], true);
        assert_eq!(span_with_text(&result, text)["dim"], false);
    }
    for text in [
        "○",
        "│ ",
        "│ │ ",
        "├─╯",
        "https://github.com/acme/widgets/pull/20",
    ] {
        assert_eq!(span_with_text(&result, text)["dim"], true, "{text}");
    }
    let title = span_with_text(&result, "Change 20");
    assert_eq!(title["fg"], Value::Null);
    assert_eq!(title["bold"], false);
    assert_eq!(title["dim"], false);
    let ansi = result["colored"].as_str().unwrap();
    for fragment in [
        "\x1b[0;35m#20",
        "\x1b[0;36mnext",
        "\x1b[0;33m[draft]",
        "\x1b[0;32m[ready]",
        "\x1b[0;1;32m@",
        "\x1b[0;2m├─╯",
        "\x1b[0;2mhttps://github.com/acme/widgets/pull/20",
        "\x1b[0mChange 20",
    ] {
        assert!(
            ansi.contains(fragment),
            "missing ANSI fragment {fragment:?}"
        );
    }
    Ok(())
}

#[test]
fn same_named_heads_in_different_repositories_stay_distinct() -> Result<()> {
    let root = pr(1, "topic", "main", 10, 1);
    let mut fork = pr(2, "topic", "topic", 20, 10);
    fork["head"]["repo"]["full_name"] = "contributor/widgets".into();
    let all = vec![root.clone(), fork];
    let result = success(plan(&root, vec![all.clone()], all))?;
    let lines = rendered_lines(&result);
    assert_eq!(lines[0], "○ #2 contributor/widgets:topic [ready]");
    assert_eq!(lines[3], "@ #1 topic [ready] (current)");
    assert_eq!(lines[6], "◆ acme/widgets:main (base)");
    Ok(())
}

#[test]
fn unrelated_siblings_and_commit_ancestry_do_not_form_a_stack() -> Result<()> {
    let current = pr(1, "topic1", "main", 10, 1);
    let mut sibling = pr(2, "topic2", "main", 20, 1);
    sibling["body"] = "Stacked on #1".into();
    let sha_only = pr(3, "topic3", "different-base", 30, 10);
    let result = success(plan(
        &current,
        vec![vec![current.clone(), sibling, sha_only]],
        vec![current.clone()],
    ))?;
    assert_eq!(result["parent"], Value::Null);
    assert_eq!(result["children"], json!([]));
    assert!(!result["render"].as_str().unwrap().contains("#2 "));
    assert!(!result["render"].as_str().unwrap().contains("#3 "));
    Ok(())
}

#[test]
fn fork_and_branch_identity_are_exact() -> Result<()> {
    let mut fork = pr(1, "topic", "main", 10, 1);
    fork["head"]["repo"]["full_name"] = "contributor/widgets".into();
    let child = pr(2, "next", "topic", 20, 10);
    let result = success(plan(
        &child,
        vec![vec![fork.clone(), child.clone()]],
        vec![child.clone()],
    ))?;
    assert_eq!(result["parent"], Value::Null);
    // A fork PR can be a child when its base points to the real upstream head.
    let parent = pr(3, "topic", "main", 10, 1);
    let mut fork_child = child.clone();
    fork_child["head"]["repo"]["full_name"] = "contributor/widgets".into();
    let all = vec![parent, fork_child.clone(), fork];
    let result = success(plan(&fork_child, vec![all.clone()], all[..2].to_vec()))?;
    assert_eq!(result["parent"], 3);
    let text = result["render"].as_str().unwrap();
    assert!(text.contains("@ #2 contributor/widgets:next [ready] (current)"));
    assert!(text.contains("○ #3 topic [ready]"));
    assert!(text.ends_with("◆ acme/widgets:main (base)\n"));
    assert!(!text.contains(" -> "));
    for (field, value) in [
        ("ref", json!("Topic")),
        ("repo", json!({"full_name": "Acme/widgets"})),
    ] {
        let mut parent = pr(3, "topic", "main", 10, 1);
        parent["head"][field] = value;
        let result = success(plan(
            &child,
            vec![vec![parent, child.clone()]],
            vec![child.clone()],
        ))?;
        assert_eq!(result["parent"], Value::Null);
    }
    Ok(())
}

#[test]
fn pagination_and_enterprise_host_are_preserved() -> Result<()> {
    let host = "github.enterprise.example";
    let mut parent = pr(200, "foundation", "main", 10, 1);
    let mut current = pr(201, "next", "foundation", 20, 10);
    for entry in [&mut parent, &mut current] {
        entry["html_url"] = format!("https://{host}/{REPO}/pull/{}", entry["number"]).into();
    }
    let mut first = vec![current.clone()];
    for n in 1..100 {
        let mut unrelated = pr(n, &format!("unrelated{n}"), "main", n + 1000, 1);
        unrelated["html_url"] = format!("https://{host}/{REPO}/pull/{n}").into();
        first.push(unrelated);
    }
    let mut p = plan(
        &current,
        vec![first, vec![parent.clone()]],
        vec![parent, current.clone()],
    );
    p["host"] = host.into();
    p["current"] = serde_json::to_value(pin(&current, host))?;
    let result = success(p)?;
    assert_eq!(result["parent"], 200);
    assert!(result["render"].as_str().unwrap().contains(host));
    Ok(())
}

#[test]
fn stale_parent_base_relationships_are_rejected() -> Result<()> {
    let parent = pr(1, "topic", "main", 11, 1);
    let current = pr(2, "next", "topic", 20, 10);
    rejected(
        plan(&current, vec![vec![parent, current.clone()]], vec![]),
        "Stale stack relationship",
    )
}

#[test]
fn duplicate_pages_ambiguous_parents_and_cycles_are_rejected() -> Result<()> {
    let current = pr(2, "next", "topic", 20, 10);
    rejected(
        plan(
            &current,
            vec![vec![current.clone()], vec![current.clone()]],
            vec![],
        ),
        "Duplicate PR #2",
    )?;
    let parent1 = pr(1, "topic", "main", 10, 1);
    let parent2 = pr(3, "topic", "main", 10, 1);
    rejected(
        plan(
            &current,
            vec![vec![parent1, current.clone(), parent2]],
            vec![],
        ),
        "Ambiguous stack parent",
    )?;
    let parent = pr(1, "topic", "next", 10, 20);
    rejected(
        plan(&current, vec![vec![parent, current.clone()]], vec![]),
        "Cycle",
    )?;
    let self_cycle = pr(1, "topic", "topic", 10, 10);
    rejected(
        plan(&self_cycle, vec![vec![self_cycle.clone()]], vec![]),
        "Cycle",
    )
}

#[test]
fn deleted_heads_are_skipped_only_when_safely_unconnected() -> Result<()> {
    let current = pr(1, "topic", "main", 10, 1);
    let mut deleted = pr(2, "unrelated", "main", 20, 1);
    deleted["head"]["repo"] = Value::Null;
    deleted["head"]["sha"] = Value::Null;
    let result = success(plan(
        &current,
        vec![vec![current.clone(), deleted.clone()]],
        vec![current.clone()],
    ))?;
    assert!(!result["render"].as_str().unwrap().contains("#2 "));
    deleted["base"]["ref"] = "topic".into();
    rejected(
        plan(
            &current,
            vec![vec![current.clone(), deleted.clone()]],
            vec![],
        ),
        "deleted head",
    )?;
    deleted["base"]["ref"] = "other".into();
    deleted["head"]["ref"] = "main".into();
    rejected(
        plan(&current, vec![vec![current.clone(), deleted]], vec![]),
        "deleted head",
    )
}

#[test]
fn current_pins_and_reviewer_are_checked() -> Result<()> {
    let current = pr(1, "topic", "main", 10, 1);
    for field in ["head_sha", "base_sha", "head_ref", "head_repo", "url"] {
        let mut p = plan(&current, vec![vec![current.clone()]], vec![current.clone()]);
        p["current"][field] = "changed".into();
        rejected(p, "changed since the session was pinned")?;
    }
    let mut p = plan(&current, vec![vec![current.clone()]], vec![current.clone()]);
    p["current"]["reviewer"] = "other".into();
    rejected(p, "does not match this client")?;
    for index in [0, 4] {
        let mut p = plan(&current, vec![vec![current.clone()]], vec![current.clone()]);
        p["steps"][index]["body"]["login"] = "Reviewer".into();
        rejected(p, "pinned reviewer changed")?;
    }
    Ok(())
}

#[test]
fn invalid_metadata_is_rejected() -> Result<()> {
    let current = pr(1, "topic", "main", 10, 1);
    for (path, replacement, message) in [
        ("/number", json!(0), "positive PR number"),
        (
            "/html_url",
            json!("https://evil.example/acme/widgets/pull/1"),
            "PR URL",
        ),
        ("/head/sha", json!("abcd"), "full hexadecimal"),
        ("/base/sha", json!("z".repeat(40)), "full hexadecimal"),
        (
            "/head/repo/full_name",
            json!("acme/widgets/extra"),
            "repository identity",
        ),
        (
            "/base/repo/full_name",
            json!("other/widgets"),
            "base repository",
        ),
        ("/head/ref", json!("topic\nspoof"), "branch ref"),
        ("/base/ref", json!("topic..other"), "branch ref"),
        ("/draft", Value::Null, "boolean draft"),
        ("/state", json!("closed"), "open and not merged"),
        ("/merged_at", json!("2026-01-01"), "open and not merged"),
    ] {
        let mut invalid = current.clone();
        *invalid.pointer_mut(path).unwrap() = replacement;
        rejected(plan(&current, vec![vec![invalid]], vec![]), message)?;
    }
    rejected(
        plan(&current, vec![vec![]], vec![]),
        "absent from the open PR listing",
    )?;
    let mut p = plan(&current, vec![vec![current.clone()]], vec![current.clone()]);
    p["steps"][2]["body"] = json!({"not": "pages"});
    rejected(p, "Expected arrays of pages")
}

#[test]
fn retargeting_and_head_changes_during_discovery_are_rejected() -> Result<()> {
    let parent = pr(1, "topic", "main", 10, 1);
    let current = pr(2, "next", "topic", 20, 10);
    let all = vec![parent.clone(), current.clone()];
    for (index, path, replacement) in [
        (2, "/body/0/1/base/ref", json!("retargeted-at-same-sha")),
        (3, "/body/head/sha", json!(oid(11))),
        (3, "/body/head/ref", json!("renamed")),
        (4, "/body/base/ref", json!("retargeted-at-same-sha")),
        (4, "/body/head/sha", json!(oid(21))),
    ] {
        let mut p = plan(&current, vec![all.clone()], all.clone());
        *p["steps"][index].pointer_mut(path).unwrap() = replacement;
        rejected(p, "changed during stack discovery")?;
    }
    Ok(())
}

fn navigation_plan(
    current: &Value,
    selected: &Value,
    all: Vec<Value>,
    refreshed: Vec<Value>,
) -> Value {
    let mut p = plan(current, vec![all.clone()], all);
    p["selected"] = selected["number"].clone();
    let mut selected = selected.clone();
    selected["merged"] = false.into(); // The individual read_pr endpoint requires this field.
    let steps = p["steps"].as_array_mut().unwrap();
    steps.extend([read(&selected), user()]);
    let new_current = refreshed
        .iter()
        .find(|pr| pr["number"] == current["number"])
        .unwrap();
    steps.extend(discovery(
        new_current,
        vec![refreshed.clone()],
        refreshed.clone(),
    ));
    p
}

#[test]
fn selected_parent_or_any_child_can_be_verified() -> Result<()> {
    let parent = pr(1, "topic", "main", 10, 1);
    let current = pr(2, "next", "topic", 20, 10);
    let sibling = pr(3, "side", "topic", 30, 10);
    let all = vec![parent.clone(), current.clone(), sibling.clone()];
    success(navigation_plan(&current, &parent, all.clone(), all.clone()))?;
    success(navigation_plan(&parent, &current, all.clone(), all.clone()))?;
    success(navigation_plan(&parent, &sibling, all.clone(), all))?;
    Ok(())
}

#[test]
fn added_child_requires_reopening_choices() -> Result<()> {
    let current = pr(1, "topic", "main", 10, 1);
    let selected = pr(2, "next", "topic", 20, 10);
    let new_child = pr(3, "side", "topic", 30, 10);
    rejected(
        navigation_plan(
            &current,
            &selected,
            vec![current.clone(), selected.clone()],
            vec![current.clone(), selected.clone(), new_child],
        ),
        "Stack child candidates changed; retry to reopen the choices",
    )
}

#[test]
fn removed_or_replaced_child_requires_reopening_choices() -> Result<()> {
    let current = pr(1, "topic", "main", 10, 1);
    let selected = pr(2, "next", "topic", 20, 10);
    let sibling = pr(3, "side", "topic", 30, 10);
    let all = vec![current.clone(), selected.clone(), sibling];
    let remaining = vec![current.clone(), selected.clone()];
    let mut replaced = remaining.clone();
    replaced.push(pr(4, "replacement", "topic", 40, 10));
    for refreshed in [remaining, replaced] {
        rejected(
            navigation_plan(&current, &selected, all.clone(), refreshed),
            "Stack child candidates changed; retry to reopen the choices",
        )?;
    }
    Ok(())
}

#[test]
fn selected_neighbor_is_revalidated_after_prompting() -> Result<()> {
    let parent = pr(1, "topic", "main", 10, 1);
    let current = pr(2, "next", "topic", 20, 10);
    let all = vec![parent.clone(), current.clone()];
    let mut new_parent = parent.clone();
    new_parent["base"]["ref"] = "retargeted-at-same-sha".into();
    rejected(
        navigation_plan(
            &current,
            &parent,
            all.clone(),
            vec![new_parent, current.clone()],
        ),
        "base branch, or identity changed",
    )?;
    let mut new_current = current.clone();
    new_current["base"]["ref"] = "no-longer-topic".into();
    let mut p = navigation_plan(
        &current,
        &parent,
        all.clone(),
        vec![parent.clone(), new_current.clone()],
    );
    // The refreshed component contains only the current PR after retargeting.
    let steps = p["steps"].as_array_mut().unwrap();
    steps.truncate(8);
    steps.extend(discovery(
        &new_current,
        vec![vec![parent.clone(), new_current.clone()]],
        vec![new_current.clone()],
    ));
    rejected(p, "Current PR base branch or identity changed")?;
    let mut changed = parent.clone();
    changed["head"]["sha"] = oid(11).into();
    rejected(
        navigation_plan(&current, &changed, all.clone(), all.clone()),
        "Selected PR #1 changed",
    )?;
    let extra_parent = pr(3, "topic", "main", 10, 1);
    rejected(
        navigation_plan(
            &current,
            &parent,
            all.clone(),
            vec![parent.clone(), current.clone(), extra_parent],
        ),
        "Ambiguous stack parent",
    )?;
    rejected(
        navigation_plan(&current, &current, all.clone(), all),
        "not an adjacent stack candidate",
    )?;
    Ok(())
}

#[test]
fn displayed_metadata_cannot_emit_terminal_controls() -> Result<()> {
    let mut current = pr(1, "topic\u{202e}evil", "main\u{2066}spoof", 10, 1);
    current["title"] = concat!(
        "Hello\u{1b}[2J\nforged\r\t\u{7}\u{9b}31m\u{202e}bad",
        "\u{1b}]8;;https://evil.example\u{7}link\u{1b}]8;;\u{7}",
        "\u{061c}\u{200e}\u{200f}\u{2028}\u{2029}\u{202a}\u{202b}\u{202c}\u{202d}",
        "\u{2066}\u{2067}\u{2068}\u{2069} café 日本語 🦀",
    )
    .into();
    let result = success(plan(
        &current,
        vec![vec![current.clone()]],
        vec![current.clone()],
    ))?;
    let text = result["render"].as_str().unwrap();
    assert!(
        !text.chars().any(|c| {
            (c.is_control() && c != '\n')
                || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    );
    assert!(text.contains("Hello\\u{1b}[2J\\nforged\\r\\t\\u{7}\\u{9b}31m\\u{202e}bad"));
    assert!(text.contains("topic\\u{202e}evil"));
    assert!(text.contains("◆ main\\u{2066}spoof (base)"));
    assert!(text.contains("\\u{1b}]8;;https://evil.example\\u{7}link"));
    assert!(text.contains("café 日本語 🦀"));
    assert_eq!(rendered_lines(&result).len(), 4);
    for line in result["lines"].as_array().unwrap() {
        assert!(!line["text"].as_str().unwrap().chars().any(char::is_control));
    }
    Ok(())
}
