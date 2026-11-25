# zed-pr-review

A Rust CLI for reviewing stacked GitHub pull requests directly inside Zed and `jj`.

Press **Ctrl-Shift-G** to open a PR. The CLI discovers the connected stack of open PRs on GitHub, fetches their commits, and builds a local stack of squashed `jj` revisions under `review/pr-<number>` bookmarks. Each review revision has the exact tree of its PR head and the right parent so Zed's normal `git::Diff` (`\ w d`) shows the full diff of the current PR.

You move up and down the stack with your normal **Ctrl-Shift-J** (`jj sb`) and **Ctrl-Shift-K** (`jj sf`) shortcuts, or click any revision in the live **Ctrl-Shift-L** (`jj l`) viewer. Select text in any file and press **Ctrl-Shift-C** to draft a local inline comment offline. Press **Ctrl-Shift-X** to finish the current PR (abandon its draft, comment, approve, request changes) or close the entire review stack.

## Setup

Install Rust **1.89 or newer** with Cargo, `jj` (0.47.1+ with JSON template metadata support), `git`, `gh`, and the `zed` CLI. Authenticate `gh` as the reviewer:

```sh
gh auth login
gh auth status
```

Install this dotfiles configuration at `~/.config/zed`, then build the release binary:

```sh
cargo build --locked --release --bin zed-pr-review --manifest-path ~/.config/zed/zed-pr-review/Cargo.toml
```

The tasks in `../tasks.json` run `~/.config/zed/zed-pr-review/target/release/zed-pr-review`. Rebuild after editing the Rust source.

## Zed Tasks and Shortcuts

| Task                  | Shortcut     | Context | Purpose                                                                                 |
| --------------------- | ------------ | ------- | --------------------------------------------------------------------------------------- |
| `PR: open for review` | Ctrl-Shift-G | Global  | Discover the PR stack, build `review/pr-*` revisions, and check out the target PR.      |
| `PR: add comment`     | Ctrl-Shift-C | Editor  | Save an inline comment for the current selection locally (offline).                     |
| `PR: finish review`   | Ctrl-Shift-X | Global  | Open the finish menu: abandon draft, comment, approve, request changes, or close stack. |
| `PR: list comments`   | None         | —       | List saved comments, receipts, and recovery status for the current PR.                  |
| `jj l`                | Ctrl-Shift-L | Global  | Live clickable `jj l` viewer with review status badges.                                 |
| `jj sb`               | Ctrl-Shift-J | Global  | Step down the stack (`jj sb`).                                                          |
| `jj sf`               | Ctrl-Shift-K | Global  | Step up the stack (`jj sf`).                                                            |
| `jj revision ready`   | Ctrl-Shift-R | Global  | Mark the checked-out PR ready for review and optionally request a reviewer.             |

## How the Review Stack Works

### Opening a PR (`Ctrl-Shift-G`)

1. Prompts for a PR number or URL (or pass `--pr <number|url>` on the CLI).
2. Discovers the connected component of open PRs on GitHub.
3. Fetches each PR's head and base history into `jj` without moving any of your existing bookmarks.
4. For each PR from bottom to top, creates (or reuses if unchanged) one squashed `jj` revision:
   - **Tree**: exact tree of the PR's `head_sha` (so multi-commit PRs appear as one clean revision).
   - **Parent**: the parent PR's review revision when the child is rebased onto it, or the real `merge_base` (with `[behind #N]` in the description) if the child is behind its parent.
   - **Bookmark**: `review/pr-<number>` (excluded from `auto-track-bookmarks = "glob:stefan/*"`).
   - **Author**: `PR Review (<gh-author>) <review@zed-pr-review.local>`, so `mine()` never matches review commits and commands like `jj rr` ignore them.
5. Saves your previous `@` commit ID in `scratch/zed-pr-review/reviews/stack.json` and runs `jj edit` on the target PR's review revision.
6. Caches each PR's changed file list and patches in `scratch/zed-pr-review/reviews/pr-<number>.json` so commenting is offline and instant.

Re-running **Ctrl-Shift-G** after new commits are pushed refreshes only the PRs that changed and preserves drafts on unchanged PRs.

### Hiding Review Noise in `jj`

`config/jj/config.toml` defines:

```toml
[revset-aliases]
"pr_reviews()" = "bookmarks(glob:'review/*')::"

[revsets]
log = "present(@) | (ancestors(immutable_heads().., 2) ~ pr_reviews()) | present(trunk())"
```

When you are working on your own branches, `jj` and `jj l` hide `review/*` commits. When `@` is on a review revision, `jj l` displays the review stack with live status badges:

- `[N pending]` (yellow) — local comments saved, not yet submitted
- `[✓ approved]` (green), `[✓ commented]` (cyan), `[✗ changes requested]` (red) — submitted receipt recorded
- `[! modified]` (red) — `@` has local edits that diverge from the PR head

### Navigating and Viewing Diffs

- Use `\ w d` (`git::Diff`) in Zed to inspect the full diff of the current PR.
- Use **Ctrl-Shift-J** (`jj sb`) and **Ctrl-Shift-K** (`jj sf`) to move down and up the review stack.
- For branching stacks with multiple children, click the child you want in **Ctrl-Shift-L** (`jj l`).

### Drafting Comments (`Ctrl-Shift-C`)

1. Select a continuous, nonblank excerpt in an editor buffer while `@` is on a review revision.
2. Press **Ctrl-Shift-C**.
3. The CLI verifies that `@` matches the unmodified review revision, checks that the file on disk matches `head_sha`, and validates that the selected lines fall within a RIGHT-side hunk of the cached PR patch.
4. Type your comment and press **Ctrl-D** on an empty line (or **Ctrl-C** / blank input to cancel).

Comments are stored locally under `scratch/zed-pr-review/<draft_key>/draft.json`. You can move up and down the stack freely, drafting comments across multiple PRs before submitting any of them.

### Finishing a Review and Closing the Stack (`Ctrl-Shift-X`)

Press **Ctrl-Shift-X** while on a review revision to open the review menu:

1. **Abandon draft** — archives and clears the local draft for the current PR; stays on the stack.
2. **Comment** — opens your summary editor (`zed --existing --wait`), verifies the PR has not changed on GitHub, submits a `COMMENT` review with all saved inline comments, and stays on the stack.
3. **Approve** — same as above with `APPROVE`; stays on the stack.
4. **Request changes** — same as above with `REQUEST_CHANGES`; stays on the stack.
5. **Close review stack** — returns `@` to your saved `previous_commit`, forgets all `review/pr-*` bookmarks, abandons all review revisions, and removes `reviews/*.json`. Drafts and submission receipts are preserved.

You can also close the review stack from anywhere in the workspace with `zed-pr-review close`.

## CLI Reference

Common options:

- `--directory <PATH>` — workspace directory (defaults to `ZED_WORKTREE_ROOT` or `.`)
- `--pr <NUMBER|URL>` — explicit PR number or URL
- `--repo <OWNER/REPO|HOST/OWNER/REPO>` — explicit repository
- `--state-dir <PATH>` — override state directory (default: `REPO_ROOT/scratch/zed-pr-review`)
- `--draft-dir <PATH>` — operate directly on a saved draft directory (`list`, `edit`, `remove`, `discard`, `recover`)
- `--editor <CMD>` — editor command for review summaries and comment edits

Subcommands:

- `open` — build or refresh the local `jj` review stack and check out the PR
- `close` — abandon all `review/pr-*` revisions, forget bookmarks, and return to previous `@`
- `stack [--watch]` — run `jj l` (or the live interactive viewer with `--watch`)
- `sb` / `sf` — run `jj sb` / `jj sf`
- `comment` — save an inline comment from Zed's selection environment variables
- `finish` — open the 5-option review menu for the current review revision
- `cancel [--yes]` — archive and clear the current PR's local draft
- `submit-comment` / `approve` / `request-changes` — submit a review directly from the CLI
- `list` — show saved comments, submission receipt, and recovery state
- `edit <INDEX>` / `remove <INDEX>` — edit or delete a saved comment (1-based index)
- `discard [--yes]` — archive and clear a local draft
- `recover [--retry]` — reconcile an uncertain submission against GitHub before retrying
- `ready [--reviewer <USER|ORG/TEAM>]` — mark the current PR ready for review

## State Layout and Submission Safety

All local state lives in `REPO_ROOT/scratch/zed-pr-review/` (excluded from Git via `.git/info/exclude` and from `jj` tracking):

```text
scratch/zed-pr-review/
├── workspace/               # workspace lock
├── reviews/
│   ├── stack.json           # previous_commit and active review PR numbers
│   └── pr-<number>.json     # change_id, commit_id, pinned PR metadata, cached patches
└── <draft_key>/
    ├── lock                 # per-draft lock
    ├── draft.json           # comments, submission attempt, retry history, receipt
    └── archive-*.json       # backups from abandoned or discarded drafts
```

Before posting a review to GitHub, the CLI checks for an existing pending review, re-validates every inline comment against the PR's current files, confirms that `head_sha` and `base_sha` have not changed, and records the submission attempt on disk. If a network error leaves the outcome uncertain, further mutations stay blocked until you run `zed-pr-review recover` to reconcile against GitHub.
