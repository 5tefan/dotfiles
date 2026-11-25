use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use zed_pr_review::feedback;
use zed_pr_review::feedback_view::{self, ReviewContext};
use zed_pr_review::github::{
    Client, checkout_sha, parse_pr_number, workspace_git_dir, workspace_root,
};
use zed_pr_review::message::{review_summary, review_template};
use zed_pr_review::model::{Comment, PullRequest};
use zed_pr_review::push::{self, PushOptions};
use zed_pr_review::ready;
use zed_pr_review::review_menu::{self, FeedbackFinishAction, ReviewAction};
use zed_pr_review::review_stack::{self, ReviewEntry};
use zed_pr_review::selection::{
    Selection, make_comment, make_comment_from_lines, repo_path, validate_comment,
};
use zed_pr_review::store::{
    Attempt, Draft, Receipt, Store, default_directory, draft_key, prepare_directory,
};

#[derive(Parser)]
#[command(
    name = "zed-pr-review",
    about = "Review GitHub pull requests from Zed and jj"
)]
struct Cli {
    /// Directory inside the jj/Git workspace (defaults to ZED_WORKTREE_ROOT or .)
    #[arg(long, global = true)]
    directory: Option<PathBuf>,
    /// Explicit PR number or URL (defaults to automatic discovery from jj @)
    #[arg(long, global = true)]
    pr: Option<String>,
    /// Explicit repository as OWNER/REPO or HOST/OWNER/REPO
    #[arg(long, global = true)]
    repo: Option<String>,
    /// Override the private draft state directory (default: REPO_ROOT/scratch/zed-pr-review)
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Operate directly on one saved draft directory (list, edit, remove, discard, recover)
    #[arg(long, global = true)]
    draft_dir: Option<PathBuf>,
    /// Editor command used for review summaries and comment edits
    #[arg(long, global = true)]
    editor: Option<String>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Build a local squashed jj review stack for a PR and check it out
    Open,
    /// Abandon all local review/pr-* revisions and return to previous @
    Close,
    /// Show the jj log with live PR review badges
    Stack {
        /// Keep the live interactive jj log open in a terminal
        #[arg(long)]
        watch: bool,
    },
    /// Step down the stack (jj sb)
    Sb,
    /// Step up the stack (jj sf)
    Sf,
    /// Mark the PR for the current jj revision ready for review and optionally request a reviewer
    Ready {
        /// GitHub username or ORG/TEAM to request as reviewer
        #[arg(long)]
        reviewer: Option<String>,
    },
    /// Push the current jj bookmark stack to origin and create or retarget GitHub PRs
    Push {
        /// Override the default stack scope with an explicit jj revset
        #[arg(short, long)]
        revset: Option<String>,
        /// Preview the push and PR plan without pushing or mutating GitHub
        #[arg(long)]
        dry_run: bool,
        /// Emit a machine-readable JSON summary of the pushed stack
        #[arg(long)]
        json: bool,
        /// Update titles of existing PRs from the first line of jj descriptions
        #[arg(short = 't', long)]
        update_title: bool,
        /// Open created/updated PR URLs in the browser
        #[arg(short, long)]
        open: bool,
        /// Create new PRs instead of updating existing ones
        #[arg(short = 'n', long)]
        force_new_pr: bool,
        /// Create new PRs as ready (or mark existing draft PRs ready)
        #[arg(long)]
        ready: bool,
        /// Allow pushing bookmarks whose commit description is empty
        #[arg(long)]
        allow_empty_description: bool,
    },
    /// Finish or abandon the current PR review, or close the review stack
    Finish,
    /// Abandon the current PR's local review draft
    Cancel {
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Submit a COMMENT-only review with any saved inline comments
    SubmitComment,
    /// Save an inline comment locally (from Zed's selection env vars or explicit CLI flags)
    Comment {
        /// File path relative to the repository root (enables non-interactive CLI mode)
        #[arg(long)]
        path: Option<String>,
        /// 1-based starting RIGHT-side line number (defaults to --line)
        #[arg(long)]
        start_line: Option<u32>,
        /// 1-based ending RIGHT-side line number
        #[arg(long)]
        line: Option<u32>,
        /// Comment body text (use `-` to read from stdin)
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read comment body text from a file
        #[arg(long, conflicts_with = "body")]
        body_file: Option<PathBuf>,
        /// Mark comment as agent-authored (rendered as blockquote + `\- an agent` when posted to GitHub)
        #[arg(long)]
        agent: bool,
    },
    /// Inspect the active local review stack offline
    ReviewInfo {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Print cached PR diff patches from the active local review stack offline
    Diff {
        /// Restrict output to one repo-relative file path
        #[arg(long)]
        file: Option<String>,
    },
    /// Submit an APPROVE review with any saved inline comments
    Approve,
    /// Submit a REQUEST_CHANGES review with any saved inline comments
    RequestChanges,
    /// List locally saved comments for the current PR
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Edit a saved comment (1-based index)
    Edit { index: usize },
    /// Remove a saved comment (1-based index)
    Remove { index: usize },
    /// Archive and clear the local draft for the current PR
    Discard {
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Reconcile an uncertain review submission against GitHub
    Recover {
        /// Explicitly allow another submission attempt after verifying GitHub
        #[arg(long)]
        retry: bool,
    },
    /// Inspect, reply to, and resolve GitHub review threads for your stack (or current review)
    Feedback {
        /// Override the default stack scope with an explicit jj revset
        #[arg(short, long)]
        revset: Option<String>,
        /// Include resolved threads in addition to unresolved threads
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        command: Option<FeedbackAction>,
    },
}

#[derive(Subcommand)]
enum FeedbackAction {
    /// Fetch review threads from GitHub and print a grouped summary
    Fetch {
        #[arg(short, long)]
        revset: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// List review threads grouped by revision / bookmark
    List {
        #[arg(short, long)]
        revset: Option<String>,
        #[arg(long)]
        all: bool,
        /// Use the last fetched local index without querying GitHub
        #[arg(long)]
        cached: bool,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Show all comments and local draft state for a thread (1-based index)
    Show {
        index: usize,
        #[arg(long)]
        all: bool,
    },
    /// Open the thread's file:line in Zed
    Open {
        index: usize,
        /// Check out the thread's jj revision before opening
        #[arg(long)]
        checkout: bool,
        #[arg(long)]
        all: bool,
    },
    /// Draft a local reply to a thread (by 1-based index or --thread ID)
    Reply {
        /// 1-based thread index (from `feedback list`, or scoped to --pr when given)
        index: Option<usize>,
        /// Target thread GraphQL node ID (optionally scoped with --pr)
        #[arg(long, conflicts_with = "index")]
        thread: Option<String>,
        /// Reply body text (use `-` to read from stdin; if omitted, reads piped stdin or opens editor)
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read reply body text from a file
        #[arg(long, conflicts_with = "body")]
        body_file: Option<PathBuf>,
        /// Mark reply as agent-authored (rendered as blockquote + `\- an agent` when published to GitHub)
        #[arg(long)]
        agent: bool,
        #[arg(long)]
        all: bool,
    },
    /// Mark a thread to be resolved locally on next publish (1-based index)
    Resolve {
        index: usize,
        #[arg(long)]
        all: bool,
    },
    /// Clear the local resolve flag on a thread (1-based index)
    Unresolve {
        index: usize,
        #[arg(long)]
        all: bool,
    },
    /// Clear staged reply and resolve flag on a thread (1-based index)
    Clear {
        index: usize,
        #[arg(long)]
        all: bool,
    },
    /// Batch-publish staged replies and resolutions to GitHub
    Publish {
        #[arg(short, long)]
        revset: Option<String>,
        /// Allow publishing even when local bookmarks differ from remote PR heads
        #[arg(long)]
        allow_unpushed: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Reconcile uncertain feedback publish attempts against GitHub
    Recover {
        /// Explicitly allow retrying remaining actions after checking GitHub
        #[arg(long)]
        retry: bool,
    },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let directory = context_directory(&cli);

    if let Action::Comment {
        path,
        start_line,
        line,
        body,
        body_file,
        agent,
    } = &cli.command
    {
        let explicit_mode = path.is_some()
            || start_line.is_some()
            || line.is_some()
            || body.is_some()
            || body_file.is_some();
        if explicit_mode {
            return comment_explicit(
                &cli,
                &directory,
                path.as_deref(),
                *start_line,
                *line,
                body.as_deref(),
                body_file.as_deref(),
                *agent,
            );
        }
        let selection = Selection::from_env().context(
            "Select one continuous, nonblank RIGHT-side excerpt in Zed; deleted-side and multiple selections are not supported",
        )?;
        return comment_local(&cli, &directory, selection, *agent);
    }
    if let Action::ReviewInfo { json } = &cli.command {
        return review_info(&cli, &directory, *json);
    }
    if let Action::Diff { file } = &cli.command {
        return diff_local(&cli, &directory, file.as_deref());
    }
    if matches!(cli.command, Action::Finish | Action::Cancel { .. }) {
        return finish_review(&cli, &directory);
    }
    if matches!(cli.command, Action::Feedback { .. }) {
        return run_feedback(&cli, &directory);
    }
    if matches!(cli.command, Action::Stack { .. } | Action::Sb | Action::Sf) {
        ensure!(
            cli.draft_dir.is_none(),
            "--draft-dir cannot be used with stack navigation"
        );
        let root = workspace_root(&directory)?;
        return navigate_local(&root, &cli.command);
    }
    if matches!(cli.command, Action::Open) {
        ensure!(
            cli.draft_dir.is_none(),
            "--draft-dir cannot be used with open"
        );
        return open_pr(&cli, &directory);
    }
    if matches!(cli.command, Action::Close) {
        ensure!(
            cli.draft_dir.is_none(),
            "--draft-dir cannot be used with close"
        );
        let root = workspace_root(&directory)?;
        let state = chosen_state(&cli, &root)?;
        let git_dir = workspace_git_dir(&root)?;
        let state = prepare_directory(&root, &git_dir, &state, None)?;
        let _workspace = Store::at(&state.join("workspace"))?;
        return review_stack::close_stack(&root, &state);
    }
    if let Action::Ready { reviewer } = &cli.command {
        ensure!(
            cli.draft_dir.is_none(),
            "--draft-dir cannot be used with ready; check out the PR head or an empty child"
        );
        let reviewer = ready::requested_reviewer(reviewer.as_deref())?;
        let root = workspace_root(&directory)?;
        let head_sha = checkout_sha(&root)?;
        println!("Captured jj @: {head_sha}");
        let (client, pr) =
            Client::discover_at(&root, cli.pr.as_deref(), cli.repo.as_deref(), &head_sha)?;
        println!("{} — {}", pr.url, printable(&pr.title));
        return ready::run(&client, &pr, reviewer.as_deref());
    }
    if let Action::Push {
        revset,
        dry_run,
        json,
        update_title,
        open,
        force_new_pr,
        ready,
        allow_empty_description,
    } = &cli.command
    {
        ensure!(
            cli.draft_dir.is_none(),
            "--draft-dir cannot be used with push"
        );
        ensure!(cli.pr.is_none(), "--pr cannot be used with push");
        let client = Client::for_workspace(&directory, cli.repo.as_deref())?;
        let opts = PushOptions {
            revset: revset.clone(),
            dry_run: *dry_run,
            json: *json,
            update_title: *update_title,
            open: *open,
            force_new_pr: *force_new_pr,
            ready: *ready,
            allow_empty_description: *allow_empty_description,
        };
        push::run(&client, &opts)?;
        return Ok(());
    }

    let list_json = matches!(cli.command, Action::List { json: true });
    let mut workspace_lock = None;
    let mut active_review: Option<ReviewEntry> = None;
    let (client, current, store, mut draft) = if let Some(path) = &cli.draft_dir {
        ensure!(
            matches!(
                cli.command,
                Action::List { .. }
                    | Action::Edit { .. }
                    | Action::Remove { .. }
                    | Action::Discard { .. }
                    | Action::Recover { .. }
            ),
            "--draft-dir is only for list, edit, remove, discard, and recover"
        );
        let store = Store::at(path)?;
        let draft = store.load()?.context("No draft.json in that directory")?;
        let pr = draft.pr.clone();
        // Recovery only uses explicit GitHub API endpoints, not local Git objects.
        let client = Client {
            root: directory.canonicalize()?,
            git_dir: PathBuf::new(),
            host: pr.host.clone(),
            repo: pr.repo.clone(),
            reviewer: pr.reviewer.clone(),
        };
        (client, pr, store, draft)
    } else {
        let root = workspace_root(&directory)?;
        let requested_state = chosen_state(&cli, &root)?;
        let git_dir = workspace_git_dir(&root)?;
        let state_dir = prepare_directory(&root, &git_dir, &requested_state, None)?;
        workspace_lock = Some(Store::at(&state_dir.join("workspace"))?);
        let (client, current, review_entry) = if matches!(cli.command, Action::List { .. })
            && let Some(entry) = resolve_active_review_entry(&cli, &root, &state_dir)?
        {
            let client = Client {
                root: root.clone(),
                git_dir: git_dir.clone(),
                host: entry.pr.host.clone(),
                repo: entry.pr.repo.clone(),
                reviewer: entry.pr.reviewer.clone(),
            };
            client.check_explicit_pr(&entry.pr, cli.pr.as_deref(), cli.repo.as_deref())?;
            let pr = entry.pr.clone();
            (client, pr, Some(entry))
        } else {
            discover_current(&cli, &root, &state_dir)?
        };
        active_review = review_entry;
        prepare_directory(&root, &git_dir, &state_dir, Some(&current.head_sha))?;
        let store = Store::open(&state_dir, &current)?;
        let draft = store.load()?.unwrap_or_else(|| Draft::new(current.clone()));
        (client, current, store, draft)
    };
    if !list_json {
        println!("{} — {}", current.url, printable(&current.title));
        println!("Draft: {}", store.directory.display());
    }
    match cli.command {
        Action::Open
        | Action::Close
        | Action::Comment { .. }
        | Action::ReviewInfo { .. }
        | Action::Diff { .. }
        | Action::Ready { .. }
        | Action::Push { .. }
        | Action::Stack { .. }
        | Action::Sb
        | Action::Sf
        | Action::Finish
        | Action::Cancel { .. }
        | Action::Feedback { .. } => {
            unreachable!("handled before opening a draft")
        }
        Action::SubmitComment => submit(
            &client,
            &store,
            &mut draft,
            &current,
            active_review.as_ref(),
            "COMMENT",
            cli.editor.as_deref(),
        )?,
        Action::Approve => submit(
            &client,
            &store,
            &mut draft,
            &current,
            active_review.as_ref(),
            "APPROVE",
            cli.editor.as_deref(),
        )?,
        Action::RequestChanges => submit(
            &client,
            &store,
            &mut draft,
            &current,
            active_review.as_ref(),
            "REQUEST_CHANGES",
            cli.editor.as_deref(),
        )?,
        Action::List { json } => list(&draft, &store.directory, json)?,
        Action::Edit { index } => {
            draft.editable()?;
            let offset = comment_index(index, &draft.comments)?;
            let body = edit_message(
                &store,
                cli.editor.as_deref(),
                &draft.comments[offset].body,
                "comment",
            )?;
            if body.trim().is_empty() {
                println!("Blank comment: edit cancelled. Original comment kept.");
            } else {
                draft.comments[offset].body = body.trim().to_owned();
                store.save(&draft)?;
                println!("Updated comment {index} locally.");
            }
        }
        Action::Remove { index } => {
            draft.editable()?;
            let offset = comment_index(index, &draft.comments)?;
            draft.comments.remove(offset);
            store.save(&draft)?;
            println!("Removed comment {index}. {} remain.", draft.comments.len());
        }
        Action::Discard { yes } => {
            ensure!(
                draft.attempt.is_none(),
                "Submission outcome is uncertain. Run recover before discarding"
            );
            if !yes && !confirm("Archive and clear this local draft? Type yes to continue: ")? {
                println!("Cancelled. Draft kept.");
                return Ok(());
            }
            let archive = store.archive(&draft)?;
            store.clear()?;
            println!(
                "Draft cleared; backup at {}. No GitHub review was changed.",
                archive.display()
            );
        }
        Action::Recover { retry } => recover(&client, &store, &mut draft, retry)?,
    }
    drop(workspace_lock);
    Ok(())
}

fn discover_current(
    cli: &Cli,
    root: &Path,
    state: &Path,
) -> Result<(Client, PullRequest, Option<ReviewEntry>)> {
    if cli.pr.is_none()
        && cli.repo.is_none()
        && let Some((entry, current_rev)) = review_stack::current_review(root, state)?
    {
        ensure!(
            current_rev.commit_id == entry.commit_id,
            "This review revision has local edits (jj @ commit changed). Run `jj restore --from {}` before submitting",
            entry.pr.head_sha
        );
        let client = Client {
            root: root.to_owned(),
            git_dir: workspace_git_dir(root)?,
            host: entry.pr.host.clone(),
            repo: entry.pr.repo.clone(),
            reviewer: entry.pr.reviewer.clone(),
        };
        let fresh_pr = client.read_pr(entry.pr.number)?;
        ensure!(
            entry.pr.same_revision(&fresh_pr),
            "PR #{} changed on GitHub since opening. Re-run ctrl-shift-g to refresh the review stack",
            entry.pr.number
        );
        return Ok((client, fresh_pr, Some(entry)));
    }
    let (client, pr) = Client::discover(root, cli.pr.as_deref(), cli.repo.as_deref())?;
    Ok((client, pr, None))
}

fn resolve_active_review_entry(
    cli: &Cli,
    root: &Path,
    state: &Path,
) -> Result<Option<ReviewEntry>> {
    if let Some(pr_spec) = cli.pr.as_deref() {
        let number = parse_pr_number(pr_spec)?;
        return review_stack::load_entry(state, number);
    }
    Ok(review_stack::current_review(root, state)?.map(|(entry, _)| entry))
}

fn trim_blank_edges(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    lines[first..=last].join("\n")
}

fn read_body_input(body: Option<&str>, body_file: Option<&Path>) -> Result<Option<String>> {
    if let Some(text) = body {
        if text == "-" {
            return Ok(Some(read_stdin()?));
        }
        return Ok(Some(text.to_owned()));
    }
    if let Some(path) = body_file {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Cannot read body file {}", path.display()))?;
        return Ok(Some(content));
    }
    if !io::stdin().is_terminal() {
        return Ok(Some(read_stdin()?));
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn comment_explicit(
    cli: &Cli,
    directory: &Path,
    path: Option<&str>,
    start_line: Option<u32>,
    line: Option<u32>,
    body: Option<&str>,
    body_file: Option<&Path>,
    agent: bool,
) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none(),
        "--draft-dir cannot be used to comment"
    );
    let path = path.context("Missing --path <REPO_RELATIVE_PATH>")?;
    let line = line.context("Missing --line <LINE>")?;
    let start_line = start_line.unwrap_or(line);
    ensure!(
        start_line > 0 && start_line <= line,
        "Invalid line range: --start-line ({start_line}) must be between 1 and --line ({line})"
    );
    let raw_body = read_body_input(body, body_file)?
        .context("Missing comment body; pass --body <TEXT>, --body-file <PATH>, or --body -")?;
    let body_text = trim_blank_edges(&raw_body);
    ensure!(!body_text.is_empty(), "Comment body must not be blank");

    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let git_dir = workspace_git_dir(&root)?;
    let state = prepare_directory(&root, &git_dir, &state, None)?;
    let _workspace = Store::at(&state.join("workspace"))?;

    let entry = if let Some(pr_spec) = cli.pr.as_deref() {
        let number = parse_pr_number(pr_spec)?;
        review_stack::load_entry(&state, number)?.with_context(|| {
            format!(
                "PR #{number} is not in the active review stack. Open it with Ctrl-Shift-G first."
            )
        })?
    } else {
        let (entry, current_rev) = review_stack::current_review(&root, &state)?.context(
            "Not on a PR review revision. Pass --pr <NUMBER> or open a PR stack with Ctrl-Shift-G first.",
        )?;
        ensure!(
            current_rev.commit_id == entry.commit_id,
            "This review revision has local edits. Run `jj restore --from {}` so line numbers match GitHub",
            entry.pr.head_sha
        );
        entry
    };

    let client = Client {
        root: root.clone(),
        git_dir,
        host: entry.pr.host.clone(),
        repo: entry.pr.repo.clone(),
        reviewer: entry.pr.reviewer.clone(),
    };
    client.check_explicit_pr(&entry.pr, cli.pr.as_deref(), cli.repo.as_deref())?;

    let store = Store::open(&state, &entry.pr)?;
    let mut draft = store
        .load()?
        .unwrap_or_else(|| Draft::new(entry.pr.clone()));
    draft.editable()?;
    draft.check_revision(&entry.pr)?;

    let blob = client.blob(&entry.pr.head_sha, path)?;
    let comment = make_comment_from_lines(
        path,
        start_line,
        line,
        &blob,
        &entry.files,
        body_text,
        agent,
    )?;

    let rendered = comment.api_value()["body"].clone();
    if draft.comments.iter().any(|existing| {
        existing.path == comment.path
            && existing.start_line == comment.start_line
            && existing.line == comment.line
            && existing.api_value()["body"] == rendered
    }) {
        bail!(
            "Duplicate comment already exists on PR #{} at {}:{}–{}",
            entry.pr.number,
            comment.path,
            comment.start_line,
            comment.line
        );
    }

    draft.comments.push(comment);
    store.save(&draft)?;
    let index = draft.comments.len();
    let range = if start_line == line {
        format!("{}:{line}", printable(path))
    } else {
        format!("{}:{start_line}-{line}", printable(path))
    };
    println!(
        "Saved comment #{index} on PR #{} ({range}){}. {index} pending; nothing published.",
        entry.pr.number,
        if agent { " [agent]" } else { "" }
    );
    Ok(())
}

fn review_info(cli: &Cli, directory: &Path, json_output: bool) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none(),
        "--draft-dir cannot be used with review-info"
    );
    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let entries = review_stack::load_ordered_entries(&state);
    let current_change = review_stack::current_revision(&root)
        .ok()
        .map(|rev| rev.change_id);

    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let draft_path = state.join(&entry.draft_key).join("draft.json");
            let draft = fs::read(&draft_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Draft>(&bytes).ok());
            let draft_comment_count = draft.as_ref().map_or(0, |d| d.comments.len());
            let submitted_state = draft
                .as_ref()
                .and_then(|d| d.submitted.as_ref().map(|r| r.state.clone()));
            let has_uncertain_attempt = draft.as_ref().is_some_and(|d| d.attempt.is_some());
            let is_current = current_change
                .as_deref()
                .is_some_and(|cid| cid == entry.change_id);
            let files: Vec<Value> = entry
                .files
                .iter()
                .map(|file| {
                    json!({
                        "filename": file.filename,
                        "status": file.status,
                        "previous_filename": file.previous_filename,
                        "has_patch": file.patch.is_some(),
                    })
                })
                .collect();
            json!({
                "number": entry.pr.number,
                "title": entry.pr.title,
                "url": entry.pr.url,
                "bookmark": entry.bookmark,
                "change_id": entry.change_id,
                "commit_id": entry.commit_id,
                "head_sha": entry.pr.head_sha,
                "base_sha": entry.pr.base_sha,
                "files": files,
                "draft_comment_count": draft_comment_count,
                "submitted_state": submitted_state,
                "has_uncertain_attempt": has_uncertain_attempt,
                "is_current": is_current,
            })
        })
        .collect();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!(
            "No active review stack. Open one with Ctrl-Shift-G or `zed-pr-review open --pr <N>`."
        );
        return Ok(());
    }
    for (entry, item) in entries.iter().zip(items.iter()) {
        let marker = if item["is_current"].as_bool().unwrap_or(false) {
            "* "
        } else {
            "  "
        };
        let count = item["draft_comment_count"].as_u64().unwrap_or(0);
        println!(
            "{marker}PR #{} ({}) — {} [{} file{}, {} draft comment{}]",
            entry.pr.number,
            entry.bookmark,
            printable(&entry.pr.title),
            entry.files.len(),
            if entry.files.len() == 1 { "" } else { "s" },
            count,
            if count == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

fn diff_local(cli: &Cli, directory: &Path, file_filter: Option<&str>) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none(),
        "--draft-dir cannot be used with diff"
    );
    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let entry = if let Some(pr_spec) = cli.pr.as_deref() {
        let number = parse_pr_number(pr_spec)?;
        review_stack::load_entry(&state, number)?.with_context(|| {
            format!(
                "PR #{number} is not in the active review stack. Open it with Ctrl-Shift-G first."
            )
        })?
    } else {
        review_stack::current_review(&root, &state)?
            .context(
                "Not on a PR review revision. Pass --pr <NUMBER> or open a PR stack with Ctrl-Shift-G first.",
            )?
            .0
    };

    let files: Vec<_> = match file_filter {
        Some(target) => {
            let matched: Vec<_> = entry
                .files
                .iter()
                .filter(|f| f.filename == target)
                .collect();
            ensure!(
                !matched.is_empty(),
                "File `{target}` is not in PR #{}'s changed files",
                entry.pr.number
            );
            matched
        }
        None => entry.files.iter().collect(),
    };

    for (idx, file) in files.iter().enumerate() {
        if idx > 0 {
            println!();
        }
        let old_name = file.previous_filename.as_deref().unwrap_or(&file.filename);
        println!("diff --git a/{old_name} b/{}", file.filename);
        if file.status == "added" {
            println!("--- /dev/null");
            println!("+++ b/{}", file.filename);
        } else if file.status == "removed" {
            println!("--- a/{old_name}");
            println!("+++ /dev/null");
        } else {
            println!("--- a/{old_name}");
            println!("+++ b/{}", file.filename);
        }
        match file.patch.as_deref() {
            Some(patch) => {
                print!("{patch}");
                if !patch.ends_with('\n') {
                    println!();
                }
            }
            None => println!("(no patch cached; binary or large diff)"),
        }
    }
    Ok(())
}

fn comment_local(cli: &Cli, directory: &Path, selection: Selection, agent: bool) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none(),
        "--draft-dir cannot be used to comment"
    );
    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let git_dir = workspace_git_dir(&root)?;
    let state = prepare_directory(&root, &git_dir, &state, None)?;
    let _workspace = Store::at(&state.join("workspace"))?;

    let (entry, current_rev) = review_stack::current_review(&root, &state)?
        .context("Not on a PR review revision. Open a PR stack with Ctrl-Shift-G first.")?;
    ensure!(
        current_rev.commit_id == entry.commit_id,
        "This review revision has local edits. Run `jj restore --from {}` so line numbers match GitHub",
        entry.pr.head_sha
    );
    let client = Client {
        root: root.clone(),
        git_dir,
        host: entry.pr.host.clone(),
        repo: entry.pr.repo.clone(),
        reviewer: entry.pr.reviewer.clone(),
    };
    client.check_explicit_pr(&entry.pr, cli.pr.as_deref(), cli.repo.as_deref())?;

    let store = Store::open(&state, &entry.pr)?;
    let mut draft = store
        .load()?
        .unwrap_or_else(|| Draft::new(entry.pr.clone()));
    draft.editable()?;
    draft.check_revision(&entry.pr)?;

    let relative_path = repo_path(&root, &selection.file)?;
    let blob = client.blob(&entry.pr.head_sha, &relative_path)?;
    let validated = make_comment(&root, &selection, &blob, &entry.files, String::new())?;
    println!("{} — {}", entry.pr.url, printable(&entry.pr.title));
    println!("Draft: {}", store.directory.display());
    println!(
        "{}:{}–{} — RIGHT",
        printable(&relative_path),
        validated.start_line,
        validated.line
    );
    println!("Selected text:\n{}", printable(&validated.selected_text));
    println!(
        "{} comments saved. Write your comment; finish with Ctrl-D on an empty line. Ctrl-C cancels.",
        draft.comments.len()
    );
    io::stdout().flush()?;
    let body = read_stdin()?;
    if body.trim().is_empty() {
        println!("Blank comment: cancelled. Existing comments kept.");
        return Ok(());
    }

    // Re-verify @ did not move while writing the comment.
    let after_rev = review_stack::current_revision(&root)?;
    ensure!(
        after_rev.change_id == entry.change_id && after_rev.commit_id == entry.commit_id,
        "jj @ changed while writing the comment; nothing saved"
    );
    let mut comment = make_comment(
        &root,
        &selection,
        &blob,
        &entry.files,
        body.trim().to_owned(),
    )?;
    comment.agent = agent;
    draft.comments.push(comment);
    store.save(&draft)?;
    println!(
        "Saved locally. {} comments pending; nothing published.",
        draft.comments.len()
    );
    Ok(())
}

fn finish_review(cli: &Cli, directory: &Path) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none() && cli.pr.is_none() && cli.repo.is_none(),
        "Finish or cancel from a review revision; --draft-dir, --pr, and --repo cannot select a different review"
    );
    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let git_dir = workspace_git_dir(&root)?;
    let state = prepare_directory(&root, &git_dir, &state, None)?;
    let _workspace = Store::at(&state.join("workspace"))?;

    let Some((entry, current_rev)) = review_stack::current_review(&root, &state)? else {
        if matches!(cli.command, Action::Finish) {
            return finish_stack_feedback(&root, &state);
        }
        anyhow::bail!(
            "Not on a PR review revision. Check out a review/pr-* revision, or run `zed-pr-review close` to clean up the stack."
        );
    };
    let store = Store::open(&state, &entry.pr)?;
    let mut draft = store
        .load()?
        .unwrap_or_else(|| Draft::new(entry.pr.clone()));

    let choice = match cli.command {
        Action::Finish => review_menu::choose(
            &format!("#{} — {}", entry.pr.number, entry.pr.title),
            draft.comments.len(),
        )?,
        Action::Cancel { yes } => {
            if yes
                || confirm(
                    "Abandon this PR's local draft? GitHub and the review stack stay unchanged. Type yes: ",
                )?
            {
                Some(ReviewAction::Cancel)
            } else {
                None
            }
        }
        _ => unreachable!("only finish and cancel use the review menu"),
    };
    let Some(choice) = choice else {
        println!("Cancelled. Review draft kept.");
        return Ok(());
    };

    if choice == ReviewAction::CloseStack {
        return review_stack::close_stack(&root, &state);
    }

    if choice == ReviewAction::Cancel {
        ensure!(
            draft.attempt.is_none(),
            "Submission outcome is uncertain. Run recover before discarding"
        );
        let archive = store.archive(&draft)?;
        store.clear()?;
        println!(
            "Local draft abandoned; backup at {}. Nothing was submitted to GitHub, and you are still on the review stack.",
            archive.display()
        );
        return Ok(());
    }

    // Re-verify @ did not move or get edited before submitting.
    let still_rev = review_stack::current_revision(&root)?;
    ensure!(
        still_rev.change_id == entry.change_id
            && still_rev.commit_id == entry.commit_id
            && current_rev.commit_id == entry.commit_id,
        "Review revision changed or has local edits; no action was taken"
    );

    let client = Client {
        root: root.clone(),
        git_dir,
        host: entry.pr.host.clone(),
        repo: entry.pr.repo.clone(),
        reviewer: entry.pr.reviewer.clone(),
    };
    let fresh_pr = client.read_pr(entry.pr.number)?;
    ensure!(
        entry.pr.same_revision(&fresh_pr),
        "PR #{} changed on GitHub since opening. Re-run ctrl-shift-g to refresh the review stack",
        entry.pr.number
    );
    let event = match choice {
        ReviewAction::Comment => "COMMENT",
        ReviewAction::Approve => "APPROVE",
        ReviewAction::RequestChanges => "REQUEST_CHANGES",
        ReviewAction::Cancel | ReviewAction::CloseStack => unreachable!(),
    };
    submit(
        &client,
        &store,
        &mut draft,
        &fresh_pr,
        Some(&entry),
        event,
        cli.editor.as_deref(),
    )?;
    // Stay on the review stack so the user can move to the next PR with ctrl-shift-k/j.
    Ok(())
}

fn finish_stack_feedback(root: &Path, state: &Path) -> Result<()> {
    let client = Client::for_workspace(root, None)?;
    let fb_dir = feedback::feedback_root(state);
    let summary = feedback::summarize_staged(&fb_dir, &client.host, &client.repo);
    if summary.staged_threads == 0 && summary.uncertain_attempts == 0 {
        println!(
            "No staged feedback replies or resolutions found. Open Ctrl-Shift-F (`zed-pr-review feedback`) to inspect and draft replies."
        );
        return Ok(());
    }
    let Some(choice) = review_menu::choose_feedback(
        &client.repo,
        summary.staged_replies,
        summary.staged_resolves,
        summary.uncertain_attempts,
    )?
    else {
        println!("Cancelled. Staged feedback drafts kept.");
        return Ok(());
    };
    match choice {
        FeedbackFinishAction::Publish => {
            feedback::publish_staged(&client, &fb_dir, None, false, false, confirm)?;
        }
        FeedbackFinishAction::Discard => {
            if confirm(
                "Archive and discard all staged feedback replies and resolve flags? GitHub stays unchanged. Type yes: ",
            )? {
                let cleared = feedback::discard_all_staged(&fb_dir)?;
                println!("Discarded staged feedback for {cleared} thread(s).");
            } else {
                println!("Cancelled. Staged feedback drafts kept.");
            }
        }
        FeedbackFinishAction::Recover => {
            feedback::recover_attempts(&client, &fb_dir, false, confirm)?;
        }
    }
    Ok(())
}

fn run_feedback(cli: &Cli, directory: &Path) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none(),
        "--draft-dir cannot be used with feedback"
    );
    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let git_dir = workspace_git_dir(&root)?;
    let state = prepare_directory(&root, &git_dir, &state, None)?;
    let fb_dir = feedback::feedback_root(&state);

    let Action::Feedback {
        revset: top_revset,
        all: top_all,
        command,
    } = &cli.command
    else {
        unreachable!();
    };

    match command {
        None => {
            // Dual-purpose Ctrl-Shift-F viewer: review stack mode vs own-stack mode.
            if cli.pr.is_none()
                && cli.repo.is_none()
                && top_revset.is_none()
                && let Some((entry, _)) = review_stack::current_review(&root, &state)?
            {
                let client = Client {
                    root: root.clone(),
                    git_dir,
                    host: entry.pr.host.clone(),
                    repo: entry.pr.repo.clone(),
                    reviewer: entry.pr.reviewer.clone(),
                };
                let threads = client.fetch_review_threads(
                    entry.pr.number,
                    &entry.pr.head_ref,
                    &entry.pr.head_sha,
                )?;
                let index = feedback::FeedbackIndex {
                    version: 1,
                    host: entry.pr.host.clone(),
                    repo: entry.pr.repo.clone(),
                    groups: vec![feedback::IndexedGroup {
                        bookmark: entry.pr.head_ref.clone(),
                        change_id: entry.change_id.clone(),
                        commit_id: entry.commit_id.clone(),
                        pr_number: entry.pr.number,
                        pr_title: entry.pr.title.clone(),
                        pr_url: entry.pr.url.clone(),
                        head_sha: entry.pr.head_sha.clone(),
                        threads,
                    }],
                };
                let review_ctx = ReviewContext {
                    draft_dir: state.join(draft_key(&entry.pr)),
                    pr: entry.pr,
                };
                return feedback_view::run(
                    &client,
                    &fb_dir,
                    index,
                    Some(review_ctx),
                    None,
                    None,
                    *top_all,
                    |store, initial, kind| {
                        edit_message(store, cli.editor.as_deref(), initial, kind)
                    },
                );
            }

            let client = Client::for_workspace(&root, cli.repo.as_deref())?;
            let index =
                feedback::fetch_stack_feedback(&client, cli.pr.as_deref(), top_revset.as_deref())?;
            feedback::save_index(&fb_dir, &index)?;
            feedback_view::run(
                &client,
                &fb_dir,
                index,
                None,
                cli.pr.as_deref(),
                top_revset.as_deref(),
                *top_all,
                |store, initial, kind| edit_message(store, cli.editor.as_deref(), initial, kind),
            )
        }
        Some(FeedbackAction::Fetch { revset, all }) => {
            let client = Client::for_workspace(&root, cli.repo.as_deref())?;
            let effective_revset = revset.as_deref().or(top_revset.as_deref());
            let index =
                feedback::fetch_stack_feedback(&client, cli.pr.as_deref(), effective_revset)?;
            feedback::save_index(&fb_dir, &index)?;
            feedback_view::print_plain_summary(&index, &fb_dir, *all || *top_all);
            Ok(())
        }
        Some(FeedbackAction::List {
            revset,
            all,
            cached,
            json,
        }) => {
            let index = if *cached {
                feedback::load_index(&fb_dir)?
                    .context("No cached feedback index; run `zed-pr-review feedback fetch` first")?
            } else {
                let client = Client::for_workspace(&root, cli.repo.as_deref())?;
                let effective_revset = revset.as_deref().or(top_revset.as_deref());
                let fetched =
                    feedback::fetch_stack_feedback(&client, cli.pr.as_deref(), effective_revset)?;
                feedback::save_index(&fb_dir, &fetched)?;
                fetched
            };
            let include_all = *all || *top_all;
            if *json {
                let threads = feedback::visible_threads(&index, include_all);
                let drafts = feedback::load_draft_map(&fb_dir);
                let items: Vec<Value> = threads
                    .iter()
                    .map(|t| {
                        let draft = drafts.get(&(t.pr_number, t.thread.id.clone()));
                        let staged_reply = draft
                            .and_then(|d| d.reply_body.as_deref())
                            .filter(|b| !b.trim().is_empty());
                        json!({
                            "index": t.index,
                            "bookmark": t.bookmark,
                            "change_id": t.change_id,
                            "pr_number": t.pr_number,
                            "pr_url": t.pr_url,
                            "thread": t.thread,
                            "staged_reply": staged_reply,
                            "staged_reply_agent": draft.is_some_and(|d| d.reply_agent && staged_reply.is_some()),
                            "staged_resolve": draft.is_some_and(|d| d.resolve),
                            "has_uncertain_attempt": draft.is_some_and(|d| d.attempt.is_some()),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                feedback_view::print_plain_summary(&index, &fb_dir, include_all);
            }
            Ok(())
        }
        Some(FeedbackAction::Show { index: idx, all }) => {
            let index = feedback::load_index(&fb_dir)?
                .context("No feedback index found; run `zed-pr-review feedback fetch` first")?;
            let item = feedback::thread_by_index(&index, *idx, *all || *top_all)?;
            let drafts = feedback::load_draft_map(&fb_dir);
            let loc = match item.thread.effective_line() {
                Some(l) => format!("{}:{l}", item.thread.path),
                None => item.thread.path.clone(),
            };
            println!(
                "[{}] PR #{} ({}) — {loc}{}{}",
                item.index,
                item.pr_number,
                item.bookmark,
                if item.thread.is_outdated {
                    " [outdated]"
                } else {
                    ""
                },
                if item.thread.is_resolved {
                    " [resolved]"
                } else {
                    ""
                }
            );
            for comment in &item.thread.comments {
                println!("\n@{} ({}):", comment.author, comment.created_at);
                println!("{}", comment.body);
            }
            if let Some(draft) = drafts.get(&(item.pr_number, item.thread.id.clone())) {
                if let Some(reply) = draft.reply_body.as_deref()
                    && !reply.trim().is_empty()
                {
                    println!(
                        "\n--- Staged reply (local){} ---\n{reply}",
                        if draft.reply_agent { " [agent]" } else { "" }
                    );
                }
                if draft.resolve {
                    println!("\n[Staged: resolve on publish]");
                }
                if draft.attempt.is_some() {
                    println!("\n[Uncertain publish attempt: run `zed-pr-review feedback recover`]");
                }
            }
            Ok(())
        }
        Some(FeedbackAction::Open {
            index: idx,
            checkout,
            all,
        }) => {
            let index = feedback::load_index(&fb_dir)?
                .context("No feedback index found; run `zed-pr-review feedback fetch` first")?;
            let item = feedback::thread_by_index(&index, *idx, *all || *top_all)?;
            let change_id = (!item.change_id.is_empty()).then_some(item.change_id.as_str());
            if let Some(warning) = feedback::open_in_zed(
                &root,
                change_id,
                &item.thread.path,
                item.thread.effective_line(),
                *checkout,
            )? {
                eprintln!("{warning}");
            }
            Ok(())
        }
        Some(FeedbackAction::Reply {
            index: idx,
            thread,
            body,
            body_file,
            agent,
            all,
        }) => {
            let index = feedback::load_index(&fb_dir)?
                .context("No feedback index found; run `zed-pr-review feedback fetch` first")?;
            let include_all = *all || *top_all;
            let pr_filter = cli.pr.as_deref().map(parse_pr_number).transpose()?;
            let item = if let Some(thread_id) = thread {
                feedback::thread_by_id(&index, pr_filter, thread_id, include_all)?
            } else if let Some(one_based) = idx {
                if let Some(pr_num) = pr_filter {
                    feedback::thread_by_pr_and_index(&index, pr_num, *one_based, include_all)?
                } else {
                    feedback::thread_by_index(&index, *one_based, include_all)?
                }
            } else {
                bail!("Provide either a thread <INDEX> or --thread <THREAD_ID>");
            };
            let non_interactive =
                body.is_some() || body_file.is_some() || !io::stdin().is_terminal();
            let (reply_text, effective_agent) =
                if let Some(raw) = read_body_input(body.as_deref(), body_file.as_deref())? {
                    (trim_blank_edges(&raw), *agent)
                } else {
                    let drafts = feedback::load_draft_map(&fb_dir);
                    let existing_draft = drafts.get(&(item.pr_number, item.thread.id.clone()));
                    let existing = existing_draft.and_then(|d| d.reply_body.as_deref());
                    let existing_agent = existing_draft.is_some_and(|d| d.reply_agent);
                    let template = feedback::reply_template(&item.thread, existing);
                    let store = feedback::open_thread_store(
                        &fb_dir,
                        &index.host,
                        &index.repo,
                        item.pr_number,
                        &item.thread.id,
                    )?;
                    let edited = edit_message(&store, cli.editor.as_deref(), &template, "reply")?;
                    (review_summary(&edited), *agent || existing_agent)
                };
            if reply_text.trim().is_empty() {
                if non_interactive {
                    bail!("Reply body must not be empty");
                }
                println!("Blank reply: cancelled. Nothing saved.");
                return Ok(());
            }
            feedback::stage_reply(
                &fb_dir,
                &index.host,
                &index.repo,
                &item.thread,
                &reply_text,
                effective_agent,
            )?;
            println!(
                "Saved local reply for [{}] PR #{} ({}){}. Nothing published yet.",
                item.index,
                item.pr_number,
                item.thread.path,
                if effective_agent { " [agent]" } else { "" }
            );
            Ok(())
        }
        Some(FeedbackAction::Resolve { index: idx, all }) => {
            let index = feedback::load_index(&fb_dir)?
                .context("No feedback index found; run `zed-pr-review feedback fetch` first")?;
            let item = feedback::thread_by_index(&index, *idx, *all || *top_all)?;
            feedback::set_resolve(&fb_dir, &index.host, &index.repo, &item.thread, true)?;
            println!(
                "Marked [{}] PR #{} ({}) to resolve on publish.",
                item.index, item.pr_number, item.thread.path
            );
            Ok(())
        }
        Some(FeedbackAction::Unresolve { index: idx, all }) => {
            let index = feedback::load_index(&fb_dir)?
                .context("No feedback index found; run `zed-pr-review feedback fetch` first")?;
            let item = feedback::thread_by_index(&index, *idx, *all || *top_all)?;
            feedback::set_resolve(&fb_dir, &index.host, &index.repo, &item.thread, false)?;
            println!(
                "Cleared resolve flag for [{}] PR #{} ({}).",
                item.index, item.pr_number, item.thread.path
            );
            Ok(())
        }
        Some(FeedbackAction::Clear { index: idx, all }) => {
            let index = feedback::load_index(&fb_dir)?
                .context("No feedback index found; run `zed-pr-review feedback fetch` first")?;
            let item = feedback::thread_by_index(&index, *idx, *all || *top_all)?;
            if feedback::clear_staged(&fb_dir, &index.host, &index.repo, &item.thread)? {
                println!(
                    "Cleared staged reply and resolve flag for [{}] PR #{} ({}).",
                    item.index, item.pr_number, item.thread.path
                );
            } else {
                println!("No staged changes on [{}].", item.index);
            }
            Ok(())
        }
        Some(FeedbackAction::Publish {
            revset,
            allow_unpushed,
            yes,
        }) => {
            let client = Client::for_workspace(&root, cli.repo.as_deref())?;
            let effective_revset = revset.as_deref().or(top_revset.as_deref());
            feedback::publish_staged(
                &client,
                &fb_dir,
                effective_revset,
                *allow_unpushed,
                *yes,
                confirm,
            )?;
            Ok(())
        }
        Some(FeedbackAction::Recover { retry }) => {
            let client = Client::for_workspace(&root, cli.repo.as_deref())?;
            feedback::recover_attempts(&client, &fb_dir, *retry, confirm)?;
            Ok(())
        }
    }
}

fn list(draft: &Draft, draft_dir: &Path, json_output: bool) -> Result<()> {
    if json_output {
        let comments: Vec<Value> = draft
            .comments
            .iter()
            .enumerate()
            .map(|(index, comment)| {
                json!({
                    "index": index + 1,
                    "path": comment.path,
                    "start_line": comment.start_line,
                    "line": comment.line,
                    "column": comment.column,
                    "selected_text": comment.selected_text,
                    "body": comment.body,
                    "agent": comment.agent,
                })
            })
            .collect();
        let output = json!({
            "pr": {
                "number": draft.pr.number,
                "title": draft.pr.title,
                "url": draft.pr.url,
                "head_sha": draft.pr.head_sha,
                "base_sha": draft.pr.base_sha,
                "reviewer": draft.pr.reviewer,
            },
            "draft_dir": draft_dir.display().to_string(),
            "submitted": draft.submitted,
            "has_uncertain_attempt": draft.attempt.is_some(),
            "comments": comments,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    println!(
        "Head: {}\nBase: {}\nReviewer: {}",
        draft.pr.head_sha, draft.pr.base_sha, draft.pr.reviewer
    );
    if let Some(receipt) = &draft.submitted {
        println!("Already submitted: {} — {}", receipt.state, receipt.url);
    }
    if draft.attempt.is_some() {
        println!("Submission outcome is uncertain. Run recover before making changes or retrying.");
    }
    println!("{} saved comments:", draft.comments.len());
    for (index, comment) in draft.comments.iter().enumerate() {
        println!(
            "\n{}. {}:{}–{} RIGHT{}\n{}",
            index + 1,
            printable(&comment.path),
            comment.start_line,
            comment.line,
            if comment.agent { " [agent]" } else { "" },
            printable(&comment.body)
        );
    }
    Ok(())
}

fn fresh(client: &Client, draft: &Draft, review_entry: Option<&ReviewEntry>) -> Result<()> {
    if let Some(entry) = review_entry {
        let rev = review_stack::current_revision(&client.root)?;
        ensure!(
            rev.change_id == entry.change_id && rev.commit_id == entry.commit_id,
            "Review revision changed or has local edits; nothing submitted"
        );
    } else {
        client.ensure_checkout(&draft.pr)?;
    }
    let current = client.read_pr(draft.pr.number)?;
    draft.check_revision(&current)
}

fn submit(
    client: &Client,
    store: &Store,
    draft: &mut Draft,
    current: &PullRequest,
    review_entry: Option<&ReviewEntry>,
    event: &str,
    editor: Option<&str>,
) -> Result<()> {
    draft.editable()?;
    if reconcile_retry_history(client, store, draft)? {
        return Ok(());
    }
    draft.check_revision(current)?;
    ensure!(
        event == "COMMENT" || !current.author.eq_ignore_ascii_case(&current.reviewer),
        "GitHub does not allow approving or requesting changes on your own PR"
    );
    ensure!(
        !client.pending_review(draft.pr.number)?,
        "You already have a pending review on GitHub. Submit or discard it there first; this tool will not alter it"
    );
    println!(
        "Decision: {event}; {} inline comments. Save and close the message tab to submit. A blank message aborts.",
        draft.comments.len()
    );
    let template = review_template(&draft.comments, |path| {
        client.blob(&draft.pr.head_sha, path)
    })?;
    let message = review_summary(&edit_message(store, editor, &template, "review")?);
    if message.trim().is_empty() {
        println!("Blank review message: aborted. All comments kept; nothing published.");
        return Ok(());
    }
    let files = client.files(draft.pr.number)?;
    for (index, comment) in draft.comments.iter().enumerate() {
        let blob = client.blob(&draft.pr.head_sha, &comment.path)?;
        validate_comment(comment, &blob, &files).with_context(|| {
            format!(
                "Comment {} is no longer valid; nothing was submitted",
                index + 1
            )
        })?;
    }
    ensure!(
        !client.pending_review(draft.pr.number)?,
        "A pending review appeared on GitHub while editing. Nothing submitted"
    );
    let previous_review_ids = client
        .reviews(draft.pr.number)?
        .iter()
        .map(|review| {
            review["id"]
                .as_u64()
                .context("Review response is missing its ID")
        })
        .collect::<Result<Vec<_>>>()?;
    // A previously uncertain POST may have become visible while the editor was open.
    if reconcile_retry_history(client, store, draft)? {
        return Ok(());
    }
    // Read the head/base as late as possible; GitHub has no atomic expected-head condition.
    fresh(client, draft, review_entry)?;
    let payload = json!({
        "commit_id": draft.pr.head_sha,
        "event": event,
        "body": message.trim(),
        "comments": draft.comments.iter().map(Comment::api_value).collect::<Vec<_>>()
    });
    draft.attempt = Some(Attempt {
        payload: payload.clone(),
        previous_review_ids,
    });
    store.save(draft)?;
    let response = client
        .submit(&draft.pr, &payload)
        .context("Draft and submission attempt preserved. Run recover; do not blindly retry")?;
    let receipt = verify_receipt(&response, &draft.pr, &payload)?;
    draft.submitted = Some(receipt);
    draft.attempt = None;
    store.save(draft).context(
        "GitHub accepted the review, but saving the receipt failed. Run recover before retrying",
    )?;
    let receipt = draft
        .submitted
        .as_ref()
        .context("Missing submission receipt")?;
    println!(
        "Submitted {} with {} comments: {}",
        receipt.state,
        draft.comments.len(),
        receipt.url
    );
    match client.read_pr(draft.pr.number) {
        Ok(after) if !draft.pr.same_revision(&after) => eprintln!(
            "Warning: the PR changed during submission. The review targets {}. Check GitHub before relying on this decision.",
            draft.pr.head_sha
        ),
        Err(_) => eprintln!(
            "Warning: could not recheck the PR after submission. The review above was submitted; do not retry it."
        ),
        _ => {}
    }
    Ok(())
}

fn verify_receipt(value: &Value, pr: &PullRequest, payload: &Value) -> Result<Receipt> {
    ensure!(
        review_matches(value, pr, payload),
        "GitHub returned an unexpected review. Outcome is uncertain; run recover before retrying"
    );
    let id = value["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .context("Review response has no valid ID; run recover")?;
    Ok(Receipt {
        id,
        url: value["html_url"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}#pullrequestreview-{id}", pr.url)),
        state: value["state"]
            .as_str()
            .context("Missing review state")?
            .to_owned(),
    })
}

fn review_matches(value: &Value, pr: &PullRequest, payload: &Value) -> bool {
    let state = match payload["event"].as_str() {
        Some("COMMENT") => "COMMENTED",
        Some("APPROVE") => "APPROVED",
        Some("REQUEST_CHANGES") => "CHANGES_REQUESTED",
        _ => return false,
    };
    (value["state"] == state || (state != "COMMENTED" && value["state"] == "DISMISSED"))
        && value["commit_id"] == pr.head_sha
        && value["body"] == payload["body"]
        && value["user"]["login"]
            .as_str()
            .is_some_and(|login| login.eq_ignore_ascii_case(&pr.reviewer))
}

fn comments_match(actual: &[Value], payload: &Value) -> bool {
    let Some(expected) = payload["comments"].as_array() else {
        return false;
    };
    if actual.len() != expected.len() {
        return false;
    }
    let mut unmatched: Vec<_> = actual.iter().collect();
    for wanted in expected {
        let Some(index) = unmatched.iter().position(|got| {
            ["path", "body", "side", "start_side"]
                .iter()
                .all(|key| got[*key] == wanted[*key])
                && got["original_commit_id"] == payload["commit_id"]
                && got["original_line"] == wanted["line"]
                && got["original_start_line"] == wanted["start_line"]
        }) else {
            return false;
        };
        unmatched.remove(index);
    }
    true
}

/// Keep the original baselines across retries so a delayed POST cannot be mistaken
/// for a review that predates our submission. Current comment coordinates may move.
fn reconcile(client: &Client, draft: &Draft) -> Result<Option<Receipt>> {
    client.authenticated_reviewer()?;
    let attempts: Vec<_> = draft
        .retry_history
        .iter()
        .chain(draft.attempt.iter())
        .collect();
    ensure!(!attempts.is_empty(), "No uncertain submission to recover");
    let mut matches = vec![];
    let mut has_new_review = false;
    for review in client.reviews(draft.pr.number)? {
        let id = review["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .context("Review response is missing its ID; recovery remains blocked")?;
        if !review["user"]["login"]
            .as_str()
            .is_some_and(|login| login.eq_ignore_ascii_case(&draft.pr.reviewer))
        {
            continue;
        }
        for attempt in &attempts {
            if attempt.previous_review_ids.contains(&id) {
                continue;
            }
            has_new_review = true;
            if review_matches(&review, &draft.pr, &attempt.payload)
                && comments_match(
                    &client.review_comments(draft.pr.number, id)?,
                    &attempt.payload,
                )
            {
                matches.push(verify_receipt(&review, &draft.pr, &attempt.payload)?);
                break;
            }
        }
    }
    ensure!(
        matches.len() <= 1,
        "Multiple matching reviews found. Inspect GitHub; draft kept locked against another submission"
    );
    if let Some(receipt) = matches.pop() {
        return Ok(Some(receipt));
    }
    ensure!(
        !has_new_review,
        "New reviews exist, but none exactly match this attempt. Inspect GitHub; no retry enabled"
    );
    Ok(None)
}

fn record_recovery(store: &Store, draft: &mut Draft, receipt: Receipt) -> Result<()> {
    println!(
        "Recovered submitted review: {} — {}",
        receipt.state, receipt.url
    );
    draft.submitted = Some(receipt);
    draft.attempt = None;
    store.save(draft)
}

fn reconcile_retry_history(client: &Client, store: &Store, draft: &mut Draft) -> Result<bool> {
    if !draft.retry_history.is_empty()
        && let Some(receipt) = reconcile(client, draft)?
    {
        record_recovery(store, draft, receipt)?;
        return Ok(true);
    }
    Ok(false)
}

fn recover(client: &Client, store: &Store, draft: &mut Draft, retry: bool) -> Result<()> {
    if let Some(receipt) = &draft.submitted {
        println!("Already submitted: {} — {}", receipt.state, receipt.url);
        return Ok(());
    }
    if let Some(receipt) = reconcile(client, draft)? {
        return record_recovery(store, draft, receipt);
    }
    if draft.attempt.is_none() {
        println!(
            "Retry is already enabled. Previous attempts will be checked again before submitting."
        );
        return Ok(());
    }
    if !retry {
        println!(
            "No matching review found. The attempt is still blocked; a delayed server response is possible. Check GitHub, then use recover --retry to explicitly enable another attempt."
        );
        return Ok(());
    }
    if confirm(
        "Confirm on GitHub that this review was NOT submitted. A delayed write could cause a duplicate. Type yes to enable a later retry: ",
    )? {
        let archive = store.archive(draft)?;
        if let Some(attempt) = draft.attempt.take() {
            draft.retry_history.push(attempt);
        }
        store.save(draft)?;
        println!(
            "Retry enabled; no review sent. Previous attempt saved at {}. Run submit-comment, approve, or request-changes again when ready.",
            archive.display()
        );
    } else {
        println!("Cancelled; uncertain attempt kept.");
    }
    Ok(())
}

fn edit_message(
    store: &Store,
    override_editor: Option<&str>,
    initial: &str,
    kind: &str,
) -> Result<String> {
    let editor = override_editor
        .map(str::to_owned)
        .or_else(|| std::env::var("ZED_PR_REVIEW_EDITOR").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vim".into());
    let args = shell_words::split(&editor).context("Invalid editor command quoting")?;
    let program = args
        .first()
        .filter(|program| !program.is_empty())
        .context("Editor command is empty")?;
    if Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "zed" | "zeditor"))
    {
        ensure!(
            args.iter().any(|arg| arg == "--wait" || arg == "-w"),
            "Zed must be invoked with --wait (for example: --editor 'zed --existing --wait')"
        );
    }
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{kind}-"))
        .suffix(".md")
        .tempfile_in(&store.directory)?;
    file.write_all(initial.as_bytes())?;
    file.as_file().sync_all()?;
    let (file, path) = file.keep()?;
    drop(file);
    println!("Message: {}", path.display());
    io::stdout().flush()?;
    let status = Command::new(program)
        .args(&args[1..])
        .arg(&path)
        .status()
        .with_context(|| format!("Could not run editor; message kept at {}", path.display()))?;
    ensure!(
        status.success(),
        "Editor failed ({status}); nothing submitted. Message kept at {}",
        path.display()
    );
    fs::read_to_string(&path)
        .with_context(|| format!("Cannot read {}; nothing submitted", path.display()))
}

fn read_stdin() -> Result<String> {
    let mut body = String::new();
    io::stdin()
        .read_to_string(&mut body)
        .context("Cannot read comment from stdin")?;
    Ok(body)
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim() == "yes")
}

fn comment_index(index: usize, comments: &[Comment]) -> Result<usize> {
    ensure!(
        index > 0 && index <= comments.len(),
        "Comment index must be between 1 and {}",
        comments.len()
    );
    Ok(index - 1)
}

fn printable(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .collect()
}

fn context_directory(cli: &Cli) -> PathBuf {
    if let Some(directory) = &cli.directory {
        return directory.clone();
    }
    if cli.draft_dir.is_none()
        && let Some(file) = std::env::var_os("ZED_FILE")
        && let Some(parent) = Path::new(&file).parent()
        && let Some(existing) = parent
            .ancestors()
            .find(|path| fs::symlink_metadata(path).is_ok())
        && let Ok(root) = workspace_root(existing)
    {
        return root;
    }
    std::env::var_os("ZED_WORKTREE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn chosen_state(cli: &Cli, root: &Path) -> Result<PathBuf> {
    let path = cli
        .state_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(|| default_directory(root))?;
    Ok(std::path::absolute(path)?)
}

fn watch_terminal(action: &Action) -> bool {
    matches!(action, Action::Stack { watch: true })
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
}

fn navigate_local(root: &Path, action: &Action) -> Result<()> {
    if watch_terminal(action) {
        return zed_pr_review::log_view::run(root);
    }
    let _workspace = if matches!(action, Action::Sb | Action::Sf)
        && root
            .join("scratch/zed-pr-review")
            .symlink_metadata()
            .is_ok()
    {
        let git_dir = workspace_git_dir(root)?;
        let state = prepare_directory(root, &git_dir, &root.join("scratch/zed-pr-review"), None)?;
        Some(Store::at(&state.join("workspace"))?)
    } else {
        None
    };
    let (program, args): (&str, &[&str]) = match action {
        Action::Stack { .. } => ("jj", &["l", "--color", "always"]),
        Action::Sb => ("jj", &["sb"]),
        Action::Sf => ("jj", &["sf"]),
        _ => unreachable!("only stack shortcuts use local navigation"),
    };
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .env("JJ_PAGER", "cat")
        .status()
        .with_context(|| format!("Cannot run normal {program} navigation"))?;
    ensure!(
        status.success(),
        "Normal {program} navigation failed ({status})"
    );
    Ok(())
}

fn open_pr(cli: &Cli, directory: &Path) -> Result<()> {
    let spec = if let Some(pr) = &cli.pr {
        pr.clone()
    } else {
        print!("PR number or URL: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        if input.is_empty() {
            println!("Cancelled. No repository changes made.");
            return Ok(());
        }
        input.to_owned()
    };
    let (client, pr) = Client::resolve(directory, &spec, cli.repo.as_deref())?;
    let requested_state = chosen_state(cli, &client.root)?;
    let state = prepare_directory(&client.root, &client.git_dir, &requested_state, None)?;
    let _workspace = Store::at(&state.join("workspace"))?;
    review_stack::open_stack(&client, &pr, &state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_compares_comment_multisets() {
        let payload = json!({
            "commit_id": "a".repeat(40),
            "comments": [
                {"path": "a.rs", "body": "one", "line": 3, "side": "RIGHT"},
                {"path": "a.rs", "body": "one", "line": 3, "side": "RIGHT"},
            ]
        });
        let comment = json!({
            "path": "a.rs", "body": "one", "side": "RIGHT",
            "original_commit_id": "a".repeat(40), "original_line": 3,
        });
        let other = json!({
            "path": "a.rs", "body": "two", "side": "RIGHT",
            "original_commit_id": "a".repeat(40), "original_line": 3,
        });
        assert!(comments_match(
            &[comment.clone(), comment.clone()],
            &payload
        ));
        assert!(!comments_match(&[comment, other], &payload));
    }
}
