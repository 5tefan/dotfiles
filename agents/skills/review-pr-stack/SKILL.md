---
name: review-pr-stack
description: Review an open zed-pr-review PR stack and draft local inline comments, or read unresolved review threads on your own stacked PRs and draft local replies. Use when asked to review a PR stack, draft PR review comments, or draft responses to PR review feedback. Strictly local only; never publishes, submits, approves, requests changes, or resolves threads on GitHub.
---

# Review a PR stack or draft feedback replies (local only)

This skill supports two local-only workflows through `zed-pr-review`:

- **Mode A**: Walk an open `zed-pr-review` PR stack from bottom to top and draft local inline review comments.
- **Mode B**: Read unresolved review threads on your own stacked PRs and draft local replies.

Invoke the CLI using `~/.config/zed/zed-pr-review/target/release/zed-pr-review` (or `zed-pr-review` when it is on `PATH`).

## Hard local-only safety boundary

Both modes are **strictly local only**, with no exceptions:

- **Never** run `submit-comment`, `approve`, `request-changes`, `finish`, `close`, `feedback publish`, or any `gh` command that posts comments, reviews, or replies, or that resolves threads on GitHub.
- **Never** edit `scratch/zed-pr-review/**/draft.json` or `scratch/zed-pr-review/feedback/**` directly. Always write through `zed-pr-review` so file locks, atomic writes, blob checks, and hunk validation stay intact.
- **Never** edit or delete pre-existing human-written draft comments or draft replies unless the user explicitly asks you to revise them.
- **Always** pass `--agent` when drafting comments or replies, and pass raw Markdown (do not manually wrap in `>` or add `\- an agent`). The CLI stores an `agent` flag locally so the user sees an `[agent]` badge in `Ctrl-Shift-F`, and deterministically renders the blockquote plus `\- an agent` signature when the user publishes to GitHub.
- **Never** move or modify the user's `jj` working copy (`@`). Inspect files at specific revisions with `JJ_PAGER=cat jj --ignore-working-copy file show -r <change_id> <path>` or `JJ_PAGER=cat jj --ignore-working-copy diff -r <change_id>`.

---

## Mode A: Reviewing someone else's PR stack

1. **Inspect the active review stack**
   Run:
   ```sh
   ~/.config/zed/zed-pr-review/target/release/zed-pr-review review-info --json
   ```
   - This returns every PR in the active review stack in bottom-to-top order with `number`, `title`, `url`, `bookmark`, `change_id`, `head_sha`, `base_sha`, `files`, `draft_comment_count`, `submitted_state`, and `is_current`.
   - If the list is empty, ask the user to open the PR stack with `Ctrl-Shift-G`. Only run `zed-pr-review open --pr <N>` if the user explicitly provided a PR number or URL and asked you to open it.

2. **Walk the stack from bottom to top**
   For each PR in bottom-to-top order:
   - Check existing local draft comments first so you preserve human-written comments and avoid duplicates:
     ```sh
     ~/.config/zed/zed-pr-review/target/release/zed-pr-review list --pr <N> --json
     ```
   - Read the PR's cached diff without changing `@` or hitting GitHub:
     ```sh
     ~/.config/zed/zed-pr-review/target/release/zed-pr-review diff --pr <N>
     ```
     Use `--file <REPO_RELATIVE_PATH>` to narrow to one file when a diff is large.
   - Read surrounding file context at the PR's revision when needed:
     ```sh
     JJ_PAGER=cat jj --ignore-working-copy file show -r <change_id> <path>
     ```

3. **Draft high-signal inline comments**
   - Focus on real bugs, logic errors, edge cases, security or data-integrity issues, and non-obvious design flaws.
   - Avoid style nitpicks, formatting remarks, or restating what the diff already shows.
   - Target valid 1-based RIGHT-side line numbers inside a single diff hunk. Do not force comments onto unchanged or deleted-only lines; note those findings in the final summary instead.
   - Save each comment locally with `--agent`:
     ```sh
     ~/.config/zed/zed-pr-review/target/release/zed-pr-review comment \
       --agent \
       --pr <N> \
       --path <REPO_RELATIVE_PATH> \
       --start-line <START> \
       --line <END> \
       --body "<RAW MARKDOWN>"
     ```
     For multi-line Markdown or code blocks, prefer `--body-file <PATH>` or `--body -` (reading from stdin) to avoid shell-quoting issues. Omit `--start-line` for single-line comments.

---

## Mode B: Responding to review feedback on your own stack

1. **Fetch and read unresolved threads**
   Run:
   ```sh
   ~/.config/zed/zed-pr-review/target/release/zed-pr-review feedback list --json
   ```
   (Use `--cached` if you just fetched and only need to re-read local state, or run `feedback fetch` first.)
   Each JSON item includes `index`, `bookmark`, `change_id`, `pr_number`, `pr_url`, `thread` (with `id`, `path`, `line`, `original_line`, `is_outdated`, `is_resolved`, and `comments`), `staged_reply`, `staged_reply_agent`, and `staged_resolve`.

2. **Inspect code context and local fixes**
   For each unresolved thread:
   - Read the thread's comments carefully.
   - Check the relevant file and revision using read-only `jj`:
     ```sh
     JJ_PAGER=cat jj --ignore-working-copy file show -r <change_id> <path>
     JJ_PAGER=cat jj --ignore-working-copy diff -r <change_id>
     ```
   - Check `staged_reply` in the JSON output. If a human-written local draft reply already exists (`staged_reply` is non-null and `staged_reply_agent` is `false`), preserve it unless the user explicitly asked to revise it.

3. **Draft local replies**
   - Write concise, accurate replies that directly answer the reviewer's question or describe verified changes.
   - If a review comment requires a code change that has not been made yet, do **not** claim it is fixed; flag it clearly in your final summary to the user (and either skip drafting a premature reply or draft an honest clarifying reply if appropriate).
   - Stage each reply locally with `--agent`:
     ```sh
     ~/.config/zed/zed-pr-review/target/release/zed-pr-review feedback reply \
       --agent \
       --pr <N> \
       --thread <THREAD_ID> \
       --body "<RAW MARKDOWN>"
     ```
     You may also use `--body-file <PATH>` or `--body -`.
   - Do not auto-resolve threads on GitHub. Only stage local resolution intent (`feedback resolve <INDEX>`) if the user explicitly asks for it.

---

## Final summary

End every run with a concise summary for the user:

1. A table of PRs inspected (PR number, bookmark/title, and count of drafted comments or replies).
2. A compact list of drafted inline comments (`PR #N path:line — 1-line summary`) or staged replies (`PR #N thread ID / path:line — 1-line summary`).
3. Any out-of-hunk observations that could not be attached inline (Mode A), or any review comments that still need code changes before replying (Mode B).
4. A short reminder that all drafts are saved locally only and nothing was posted to GitHub; the user can review them in `Ctrl-Shift-F` and submit or publish via `Ctrl-Shift-X`.
