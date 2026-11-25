use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use zed_pr_review::github::{Client, checkout_sha, workspace_git_dir, workspace_root};
use zed_pr_review::message::{review_summary, review_template};
use zed_pr_review::model::{Comment, PullRequest};
use zed_pr_review::ready;
use zed_pr_review::review_menu::{self, ReviewAction};
use zed_pr_review::review_stack::{self, ReviewEntry};
use zed_pr_review::selection::{Selection, make_comment, repo_path, validate_comment};
use zed_pr_review::store::{Attempt, Draft, Receipt, Store, default_directory, prepare_directory};

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
    /// Save an inline comment from Zed's current selection locally
    Comment,
    /// Submit an APPROVE review with any saved inline comments
    Approve,
    /// Submit a REQUEST_CHANGES review with any saved inline comments
    RequestChanges,
    /// List locally saved comments for the current PR
    List,
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
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let selection = if matches!(cli.command, Action::Comment) {
        Some(Selection::from_env().context("Select one continuous, nonblank RIGHT-side excerpt in Zed; deleted-side and multiple selections are not supported")?)
    } else {
        None
    };
    let directory = context_directory(&cli);

    if matches!(cli.command, Action::Comment) {
        return comment_local(&cli, &directory, selection.context("Missing selection")?);
    }
    if matches!(cli.command, Action::Finish | Action::Cancel { .. }) {
        return finish_review(&cli, &directory);
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

    let mut workspace_lock = None;
    let mut active_review: Option<ReviewEntry> = None;
    let (client, current, store, mut draft) = if let Some(path) = &cli.draft_dir {
        ensure!(
            matches!(
                cli.command,
                Action::List
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
        let (client, current, review_entry) = discover_current(&cli, &root, &state_dir)?;
        active_review = review_entry;
        prepare_directory(&root, &git_dir, &state_dir, Some(&current.head_sha))?;
        let store = Store::open(&state_dir, &current)?;
        let draft = store.load()?.unwrap_or_else(|| Draft::new(current.clone()));
        (client, current, store, draft)
    };
    println!("{} — {}", current.url, printable(&current.title));
    println!("Draft: {}", store.directory.display());
    match cli.command {
        Action::Open
        | Action::Close
        | Action::Comment
        | Action::Ready { .. }
        | Action::Stack { .. }
        | Action::Sb
        | Action::Sf
        | Action::Finish
        | Action::Cancel { .. } => {
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
        Action::List => list(&draft),
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

fn comment_local(cli: &Cli, directory: &Path, selection: Selection) -> Result<()> {
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
    let comment = make_comment(
        &root,
        &selection,
        &blob,
        &entry.files,
        body.trim().to_owned(),
    )?;
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

    let (entry, current_rev) = review_stack::current_review(&root, &state)?.context(
        "Not on a PR review revision. Check out a review/pr-* revision, or run `zed-pr-review close` to clean up the stack.",
    )?;
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

fn list(draft: &Draft) {
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
            "\n{}. {}:{}–{} RIGHT\n{}",
            index + 1,
            printable(&comment.path),
            comment.start_line,
            comment.line,
            printable(&comment.body)
        );
    }
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
