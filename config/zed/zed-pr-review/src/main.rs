use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zed_pr_review::github::{Client, checkout_sha, workspace_git_dir, workspace_root};
use zed_pr_review::message::{review_summary, review_template};
use zed_pr_review::model::{Comment, PullRequest};
use zed_pr_review::ready;
use zed_pr_review::review_menu::{self, ReviewAction};
use zed_pr_review::selection::{Selection, make_comment, validate_comment};
use zed_pr_review::session::{self, Session};
use zed_pr_review::stack::Stack;
use zed_pr_review::store::{Attempt, Draft, Receipt, Store, default_directory, prepare_directory};

#[derive(Parser)]
#[command(
    version,
    about = "Review GitHub PRs from Zed, or mark the checked-out PR ready for review"
)]
struct Cli {
    /// jj workspace; defaults to ZED_WORKTREE_ROOT, then the current directory.
    #[arg(long, global = true)]
    directory: Option<PathBuf>,
    /// Explicit PR number or https://HOST/owner/repo/pull/NUMBER.
    #[arg(long, global = true)]
    pr: Option<String>,
    /// Override the base repository: [HOST/]owner/repo.
    #[arg(long, global = true)]
    repo: Option<String>,
    /// Override private drafts; defaults to jj root/scratch/zed-pr-review.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Open a particular draft directory, even after the PR/checkout changes.
    #[arg(long, global = true, conflicts_with_all = ["pr", "repo"])]
    draft_dir: Option<PathBuf>,
    /// Editor command with optional arguments (no shell evaluation).
    #[arg(long, global = true)]
    editor: Option<String>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Fetch and check out a PR, then open its full diff. Prompts if --pr is omitted.
    Open,
    /// Show the GitHub stack in a review diff; otherwise run jj l.
    Stack {
        /// Keep the PR stack or local jj log open and refresh it; non-terminal output is one-shot.
        #[arg(long)]
        watch: bool,
    },
    /// Open the parent PR in review mode; otherwise run jj sb.
    Sb,
    /// Open a child PR in review mode; otherwise run jj sf.
    Sf,
    /// Mark the PR ready from its head or empty child; optionally request a reviewer.
    Ready {
        /// GitHub login or organization/team-slug; defaults to ZED_REVISION_READY_REVIEWER.
        #[arg(long)]
        reviewer: Option<String>,
    },
    /// Choose how to finish: abandon locally, comment, approve, or request changes.
    Finish,
    /// Abandon the local review draft and snapshots without contacting GitHub.
    Cancel {
        #[arg(long)]
        yes: bool,
    },
    /// Submit a comment-only review with every saved inline comment.
    SubmitComment,
    /// Save a comment for one selected RIGHT-side excerpt. Read the body until EOF.
    Comment,
    /// Edit a nonblank summary, then approve with every saved inline comment.
    Approve,
    /// Edit a nonblank summary, then request changes with every saved comment.
    RequestChanges,
    /// Show saved comments, revision, recovery status, and draft directory.
    List,
    /// Edit the body of a saved comment (one-based index).
    Edit { index: usize },
    /// Remove a saved comment (one-based index).
    Remove { index: usize },
    /// Archive and clear a draft. Does not remove reviews from GitHub.
    Discard {
        #[arg(long)]
        yes: bool,
    },
    /// Reconcile an uncertain submission without sending another review.
    Recover {
        /// If no new review exists, allow a retry after explicit confirmation.
        #[arg(long)]
        retry: bool,
    },
    /// Open a frozen full-PR comparison with comments from registered RHS snapshots.
    Diff,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("zed-pr-review: {error:#}");
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
    if matches!(cli.command, Action::Finish | Action::Cancel { .. }) {
        return finish_review(&cli, &directory);
    }
    if matches!(cli.command, Action::Stack { .. } | Action::Sb | Action::Sf) {
        return navigate(&cli, &directory);
    }
    if matches!(cli.command, Action::Open) {
        ensure!(
            cli.draft_dir.is_none(),
            "--draft-dir cannot be used with open"
        );
        return open_pr(&cli, &directory);
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
        let state = chosen_state(&cli, &root)?;
        let has_session =
            state == root.join("scratch/zed-pr-review") && state.join("active.json").exists();
        let (client, pr) = if has_session {
            let git_dir = workspace_git_dir(&root)?;
            let state = prepare_directory(&root, &git_dir, &state, None)?;
            let selected = selected_session(&root, &state, false)?;
            let (client, current) =
                discover_session(&cli, &root, selected.as_ref(), Some(&head_sha))?;
            (client, current)
        } else {
            Client::discover_at(&root, cli.pr.as_deref(), cli.repo.as_deref(), &head_sha)?
        };
        println!("{} — {}", pr.url, printable(&pr.title));
        return ready::run(&client, &pr, reviewer.as_deref());
    }
    let requires_session = matches!(
        cli.command,
        Action::Comment | Action::SubmitComment | Action::Approve | Action::RequestChanges
    );
    let mut review_session = None;
    let mut workspace_lock = None;
    let mut state_base = None;
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
            "--draft-dir is only for list, edit, remove, discard, and recover; open a full PR diff to comment or submit"
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
        if requires_session {
            // Refuse outside the diff before creating state, acquiring locks,
            // discovering a PR, or interacting with stdin/an editor.
            require_review_session(
                &root,
                &requested_state,
                matches!(cli.command, Action::Comment),
            )?;
        }
        let git_dir = workspace_git_dir(&root)?;
        if matches!(cli.command, Action::Diff) {
            ensure!(
                requested_state == root.join("scratch/zed-pr-review"),
                "Full diff sessions require REPO_ROOT/scratch/zed-pr-review; omit the external state override"
            );
        }
        let state_dir = prepare_directory(&root, &git_dir, &requested_state, None)?;
        workspace_lock = Some(Store::at(&state_dir.join("workspace"))?);
        review_session = if requires_session {
            Some(require_review_session(
                &root,
                &state_dir,
                matches!(cli.command, Action::Comment),
            )?)
        } else {
            selected_session(&root, &state_dir, false)?
        };
        let (client, current) = discover_session(&cli, &root, review_session.as_ref(), None)?;
        prepare_directory(&root, &git_dir, &state_dir, Some(&current.head_sha))?;
        let store = Store::open(&state_dir, &current)?;
        let draft = store.load()?.unwrap_or_else(|| Draft::new(current.clone()));
        state_base = Some(state_dir);
        (client, current, store, draft)
    };
    println!("{} — {}", current.url, printable(&current.title));
    println!("Draft: {}", store.directory.display());
    let finish_review = matches!(
        cli.command,
        Action::SubmitComment | Action::Approve | Action::RequestChanges | Action::Recover { .. }
    );
    match cli.command {
        Action::Open
        | Action::Ready { .. }
        | Action::Stack { .. }
        | Action::Sb
        | Action::Sf
        | Action::Finish
        | Action::Cancel { .. } => {
            unreachable!(
                "checkout, navigation, ready, and the finish menu return before opening a draft"
            )
        }
        Action::Comment => {
            draft.editable()?;
            draft.check_revision(&current)?;
            let selection = selection.context("Missing selection")?;
            let session = review_session.as_ref().context("Missing review session")?;
            let path = session.selection_path(&selection.file)?;
            let comment_root = session.head_root();
            let blob = client.blob(&draft.pr.head_sha, &path)?;
            let files = client.files(current.number)?;
            let validated = make_comment(&comment_root, &selection, &blob, &files, String::new())?;
            println!(
                "{}:{}–{} — RIGHT",
                printable(&path),
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
            fresh(&client, &draft)?;
            session.selection_path(&selection.file)?;
            if let Some(state) = &state_base {
                prepare_directory(
                    &client.root,
                    &client.git_dir,
                    state,
                    Some(&draft.pr.head_sha),
                )?;
            }
            let comment = make_comment(
                &comment_root,
                &selection,
                &blob,
                &files,
                body.trim().to_owned(),
            )?;
            draft.comments.push(comment);
            store.save(&draft)?;
            println!(
                "Saved locally. {} comments pending; nothing published.",
                draft.comments.len()
            );
        }
        Action::SubmitComment => submit(
            &client,
            &store,
            &mut draft,
            &current,
            "COMMENT",
            cli.editor.as_deref(),
        )?,
        Action::Approve => submit(
            &client,
            &store,
            &mut draft,
            &current,
            "APPROVE",
            cli.editor.as_deref(),
        )?,
        Action::RequestChanges => submit(
            &client,
            &store,
            &mut draft,
            &current,
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
        Action::Diff => {
            let state = state_base
                .as_ref()
                .context("Missing review state directory")?;
            ensure!(
                state == &client.root.join("scratch/zed-pr-review"),
                "Full diff sessions require REPO_ROOT/scratch/zed-pr-review; omit the external state override"
            );
            let session = Session::prepare(&client, &store, &current)?;
            client.ensure_checkout(&current)?;
            session.activate(state)?;
            session.open()?;
        }
    }
    if finish_review && draft.submitted.is_some() {
        cleanup_after_review(&client, &store, &draft, workspace_lock.is_some());
    }
    drop(workspace_lock);
    Ok(())
}

fn finish_review(cli: &Cli, directory: &Path) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none() && cli.pr.is_none() && cli.repo.is_none(),
        "Finish or cancel from the selected full PR diff; --draft-dir, --pr, and --repo cannot select a different review"
    );
    let root = workspace_root(directory)?;
    let state = chosen_state(cli, &root)?;
    let selected = cancellation_session(&root, &state)?;
    let git_dir = workspace_git_dir(&root)?;
    let state = prepare_directory(&root, &git_dir, &state, None)?;
    let _workspace = Store::at(&state.join("workspace"))?;
    let locked = cancellation_session(&root, &state)?;
    ensure!(
        locked.directory == selected.directory && locked.pr.same_revision(&selected.pr),
        "Review context changed before opening the menu; retry from its diff"
    );
    let store = Store::open(&state, &locked.pr)?;
    let mut draft = store
        .load()?
        .unwrap_or_else(|| Draft::new(locked.pr.clone()));
    let choice = match cli.command {
        Action::Finish => review_menu::choose(
            &format!("#{} — {}", locked.pr.number, locked.pr.title),
            draft.comments.len(),
        )?,
        Action::Cancel { yes } => {
            if yes
                || confirm(
                    "Abandon this local review and clean its snapshots? GitHub and the checkout stay unchanged. Type yes: ",
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
        println!("Cancelled. Review draft and snapshots kept.");
        return Ok(());
    };
    let still_selected = cancellation_session(&root, &state)?;
    ensure!(
        still_selected.directory == locked.directory && still_selected.pr.same_revision(&locked.pr),
        "Review context changed while choosing; no action was taken"
    );
    if choice == ReviewAction::Cancel {
        let count = session::cancel_review(&root, &store, &draft)?;
        println!(
            "Review abandoned locally. Cleaned {count} sessions; the draft was archived in {}. Nothing was submitted to GitHub, and the checkout is unchanged.",
            store.directory.display()
        );
        return Ok(());
    }

    // Cancel works offline even after a checkout or remote PR changes. Every
    // submission still requires a live diff and the normal pinned revision checks.
    still_selected.validate()?;
    let (client, current) = discover_session(cli, &root, Some(&still_selected), None)?;
    client.ensure_checkout(&current)?;
    prepare_directory(&root, &git_dir, &state, Some(&current.head_sha))?;
    let event = match choice {
        ReviewAction::Comment => "COMMENT",
        ReviewAction::Approve => "APPROVE",
        ReviewAction::RequestChanges => "REQUEST_CHANGES",
        ReviewAction::Cancel => unreachable!(),
    };
    submit(
        &client,
        &store,
        &mut draft,
        &current,
        event,
        cli.editor.as_deref(),
    )?;
    if draft.submitted.is_some() {
        cleanup_after_review(&client, &store, &draft, true);
    }
    Ok(())
}

fn cancellation_session(root: &Path, state: &Path) -> Result<Session> {
    ensure!(
        state == root.join("scratch/zed-pr-review"),
        "Finishing or cancelling requires a full PR diff in REPO_ROOT/scratch/zed-pr-review"
    );
    let file = std::env::var_os("ZED_FILE").filter(|file| !file.is_empty())
        .context("Not inside a review session. Open a PR with Ctrl-Shift-G and finish from its full diff")?;
    session::load_for_cancel(root, state, Path::new(&file))?
        .context("Not inside a review session. Finish from a file in the full PR diff, not an ordinary source file")
}

fn cleanup_after_review(client: &Client, store: &Store, draft: &Draft, workspace_locked: bool) {
    let Some(root) = store.directory.ancestors().nth(3) else {
        return;
    };
    if store.directory
        != root
            .join("scratch/zed-pr-review")
            .join(zed_pr_review::store::draft_key(&draft.pr))
    {
        return;
    }
    let cleanup = || -> Result<usize> {
        let _workspace;
        let git_dir;
        if workspace_locked {
            ensure!(
                root == client.root,
                "Cleanup workspace differs from the locked workspace"
            );
            git_dir = client.git_dir.clone();
            _workspace = None;
        } else {
            // --draft-dir recovery can run from another checkout. Derive the
            // original workspace from the store, never from its selected file.
            ensure!(
                workspace_root(root)? == root,
                "Cleanup requires the original jj workspace"
            );
            git_dir = workspace_git_dir(root)?;
            let state =
                prepare_directory(root, &git_dir, &root.join("scratch/zed-pr-review"), None)?;
            _workspace = Some(Store::at(&state.join("workspace"))?);
        }
        prepare_directory(root, &git_dir, &root.join("scratch/zed-pr-review"), None)?;
        session::cleanup_submitted(root, store, draft)
    };
    match cleanup() {
        Ok(0) => {}
        Ok(count) => println!(
            "Cleaned head/base snapshots from {count} review sessions. Comments, receipts, and stack navigation metadata were kept."
        ),
        Err(error) => eprintln!(
            "Warning: the review is already submitted, but snapshot cleanup failed: {error:#}. Some snapshots may remain. Run recover with --draft-dir pointing to {} to retry cleanup; do not submit the review again.",
            store.directory.display()
        ),
    }
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

fn fresh(client: &Client, draft: &Draft) -> Result<()> {
    client.ensure_checkout(&draft.pr)?;
    let current = client.read_pr(draft.pr.number)?;
    draft.check_revision(&current)
}

fn submit(
    client: &Client,
    store: &Store,
    draft: &mut Draft,
    current: &PullRequest,
    event: &str,
    editor: Option<&str>,
) -> Result<()> {
    draft.editable()?;
    if reconcile_retry_history(client, store, draft)? {
        return Ok(());
    }
    draft.check_revision(current)?;
    ensure!(
        event == "COMMENT" || !draft.pr.author.eq_ignore_ascii_case(&draft.pr.reviewer),
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
    fresh(client, draft)?;
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
                '�'
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
    // In a multi-root Zed window, the selected buffer is a better workspace hint
    // than the visible-root fallback. This only resolves context, never trusts a snapshot.
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

fn require_review_session(root: &Path, state: &Path, commenting: bool) -> Result<Session> {
    ensure!(
        state == root.join("scratch/zed-pr-review"),
        "Comment, approve, and request-changes require a full PR diff session in REPO_ROOT/scratch/zed-pr-review; omit the external state override"
    );
    let file = std::env::var_os("ZED_FILE")
        .filter(|file| !file.is_empty())
        .context("Not inside a review session. Open a PR with Ctrl-Shift-G and run this command from its full diff")?;
    let selected = if commenting {
        session::load_for_file(root, state, Path::new(&file))?
    } else {
        session::load_for_navigation(root, state, Path::new(&file))?
    };
    let selected = selected.context("Not inside a review session. Use a file in the full PR diff, not an ordinary source file; open the PR with Ctrl-Shift-G")?;
    selected.validate()?;
    Ok(selected)
}

fn selected_session(root: &Path, state: &Path, commenting: bool) -> Result<Option<Session>> {
    let local = root.join("scratch/zed-pr-review");
    if let Some(file) = std::env::var_os("ZED_FILE") {
        let file = PathBuf::from(file);
        if file.starts_with(&local) {
            ensure!(
                state == local,
                "Snapshot review uses REPO_ROOT/scratch/zed-pr-review; omit the external state override"
            );
        }
        if state == local {
            let selected = if commenting {
                session::load_for_file(root, state, &file)?
            } else {
                session::load_for_navigation(root, state, &file)?
            };
            if selected.is_some() {
                return Ok(selected);
            }
        }
    }
    if state == local {
        session::load_active(root, state)
    } else {
        Ok(None)
    }
}

fn discover_session(
    cli: &Cli,
    root: &Path,
    session: Option<&Session>,
    captured_head: Option<&str>,
) -> Result<(Client, PullRequest)> {
    let pr = cli
        .pr
        .as_deref()
        .or_else(|| session.map(|session| session.pr.url.as_str()));
    let (client, current) = match captured_head {
        Some(head) => Client::discover_at(root, pr, cli.repo.as_deref(), head)?,
        None => Client::discover(root, pr, cli.repo.as_deref())?,
    };
    if let Some(session) = session {
        ensure!(
            session.pr.same_revision(&current),
            "This review session is stale or belongs to a different PR. Open the PR again; old diff tabs cannot be rebound to a new revision"
        );
    }
    Ok((client, current))
}

fn navigation_session(root: &Path) -> Result<Option<Session>> {
    let state = root.join("scratch/zed-pr-review");
    if let Some(file) = std::env::var_os("ZED_FILE").filter(|file| !file.is_empty()) {
        // An ordinary source buffer is always jj mode, even with a saved session.
        return session::load_for_navigation(root, &state, Path::new(&file));
    }
    let Some(active) = session::load_active(root, &state)? else {
        return Ok(None);
    };
    let client = Client {
        root: root.to_owned(),
        git_dir: workspace_git_dir(root)?,
        host: active.pr.host.clone(),
        repo: active.pr.repo.clone(),
        reviewer: active.pr.reviewer.clone(),
    };
    // Terminal/task context has no snapshot path. Keep review mode for the pinned
    // head or its unchanged child, but propagate inspection failures rather than navigate locally.
    Ok(client.checkout_matches(&active.pr)?.then_some(active))
}

fn navigate(cli: &Cli, directory: &Path) -> Result<()> {
    ensure!(
        cli.draft_dir.is_none(),
        "--draft-dir cannot be used with stack navigation"
    );
    let root = workspace_root(directory)?;
    let Some(selected) = navigation_session(&root)? else {
        ensure!(
            cli.pr.is_none() && cli.repo.is_none(),
            "Open the PR first to navigate its stack; --pr and --repo cannot turn a source buffer into review mode"
        );
        return navigate_local(&root, &cli.command);
    };
    let state = chosen_state(cli, &root)?;
    ensure!(
        state == root.join("scratch/zed-pr-review"),
        "Review stack navigation requires REPO_ROOT/scratch/zed-pr-review; omit the external state override"
    );
    let git_dir = workspace_git_dir(&root)?;
    let state = prepare_directory(&root, &git_dir, &state, None)?;
    if watch_terminal(&cli.command) {
        let pr = cli.pr.clone();
        let repo = cli.repo.clone();
        let mut origin_validated = false;
        return zed_pr_review::stack_view::run(move || {
            // Validate the originating diff first. After that, the read-only
            // viewer may follow a new active review, but never rebinds old diffs.
            let active = if origin_validated && pr.is_none() {
                session::load_active(&root, &state)?
            } else {
                None
            };
            let pinned = active.as_ref().unwrap_or(&selected);
            let spec = pr.as_deref().unwrap_or(&pinned.pr.url);
            let (client, current) = Client::discover(&root, Some(spec), repo.as_deref())?;
            ensure!(
                pinned.pr.same_revision(&current),
                "This review session is stale. Open the PR again to refresh its pins"
            );
            client.ensure_checkout(&current)?;
            // Navigation may change @ during discovery. Once the origin passed
            // its checks, the next read can follow that newly active review.
            origin_validated = true;
            let stack = Stack::discover(&client, &current)?;
            client.ensure_checkout(&current)?;
            Ok(stack)
        });
    }
    let _workspace = Store::at(&state.join("workspace"))?;
    let (client, current) = discover_session(cli, &root, Some(&selected), None)?;
    let stack = Stack::discover(&client, &current)?;
    client.ensure_checkout(&current)?;
    println!("PR stack — {}/{}\n", current.host, current.repo);
    print!(
        "{}",
        if io::stdout().is_terminal() {
            stack.render_colored()
        } else {
            stack.render()
        }
    );
    if matches!(cli.command, Action::Stack { .. }) {
        println!(
            "Ctrl-Shift-J: parent PR. Ctrl-Shift-K: child PR. Ctrl-Shift-L: refresh this list."
        );
        return Ok(());
    }
    let candidates = if matches!(cli.command, Action::Sb) {
        stack.parent()?.into_iter().collect::<Vec<_>>()
    } else {
        stack.children()
    };
    if candidates.is_empty() {
        println!(
            "No open {} PR in this stack. The checkout and drafts are unchanged.",
            if matches!(cli.command, Action::Sb) {
                "parent"
            } else {
                "child"
            }
        );
        return Ok(());
    }
    let candidate = if candidates.len() == 1 {
        candidates[0]
    } else {
        println!("More than one child PR is available:");
        for entry in &candidates {
            println!("  #{} — {}", entry.pr.number, printable(&entry.pr.title));
        }
        print!("PR number (blank cancels): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        if input.is_empty() {
            println!("Cancelled. The checkout and drafts are unchanged.");
            return Ok(());
        }
        let number: u64 = input
            .parse()
            .context("Enter one of the listed PR numbers")?;
        candidates
            .into_iter()
            .find(|entry| entry.pr.number == number)
            .context("Enter one of the listed PR numbers; no checkout was attempted")?
    };
    let still_selected =
        navigation_session(&root)?.context("Review context changed; retry from its diff")?;
    ensure!(
        still_selected.directory == selected.directory && still_selected.pr.same_revision(&current),
        "Review context changed while selecting a PR; retry from its diff"
    );
    client.ensure_checkout(&current)?;
    let target = client.read_pr(candidate.pr.number)?;
    stack.verify_adjacent(&client, &target)?;
    checkout_review(&client, &target, &state, Some(&stack))
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
    ensure!(
        requested_state == client.root.join("scratch/zed-pr-review"),
        "Opening a PR requires REPO_ROOT/scratch/zed-pr-review; omit the external state override"
    );
    let state = prepare_directory(&client.root, &client.git_dir, &requested_state, None)?;
    let _workspace = Store::at(&state.join("workspace"))?;
    checkout_review(&client, &pr, &state, None)
}

fn checkout_review(
    client: &Client,
    pr: &PullRequest,
    state: &Path,
    stack: Option<&Stack>,
) -> Result<()> {
    println!("{} — {}", pr.url, printable(&pr.title));
    println!("Fetching PR head {} and base history…", pr.head_sha);
    client.fetch_pr(pr)?;
    prepare_directory(&client.root, &client.git_dir, state, Some(&pr.head_sha))?;
    let store = Store::open(state, pr)?;
    let session = Session::prepare(client, &store, pr)?;
    if let Some(stack) = stack {
        client.ensure_checkout(&stack.current().pr)?;
        stack.verify_adjacent(client, pr)?;
    }
    let previous_commit = client.checkout_pr(pr)?;
    // Report recovery before any later validation or state write can fail.
    println!("Previous work is preserved. Return with: jj edit {previous_commit}");
    println!("If that revision is immutable, use: jj new {previous_commit}");
    prepare_directory(&client.root, &client.git_dir, state, Some(&pr.head_sha))?;
    store.write_json(
        "last-checkout.json",
        &json!({
            "pr": pr, "previous_commit": previous_commit, "session": session.directory,
        }),
    )?;
    client.ensure_checkout(pr)?;
    ensure!(
        pr.same_revision(&client.read_pr(pr.number)?),
        "PR changed after checkout. Previous work is preserved; open the PR again"
    );
    session.activate(state)?;
    session.open().context("Checkout succeeded, but the diff could not be opened. Run the PR: full diff task; do not repeat checkout just to reopen the view")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_compares_comment_multisets() {
        let comment = json!({"path":"x", "body":"text", "line":1,"side":"RIGHT"});
        let payload = json!({"commit_id":"head", "comments":[comment.clone(), comment.clone()]});
        let mut comment = comment;
        comment["original_commit_id"] = json!("head");
        comment["original_line"] = json!(1);
        assert!(comments_match(
            &[comment.clone(), comment.clone()],
            &payload
        ));
        assert!(!comments_match(std::slice::from_ref(&comment), &payload));
        let mut different = comment.clone();
        different["side"] = "LEFT".into();
        assert!(!comments_match(&[comment, different], &payload));
    }
}
