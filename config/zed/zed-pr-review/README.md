# zed-pr-review

A Rust CLI for reviewing GitHub pull requests from Zed and a jj workspace.
Press **Ctrl-Shift-G** to open a PR: the CLI fetches it, preserves your local work,
checks out the PR's files, and opens the full comparison. Select right-side text
and press **Ctrl-Shift-C** to save an inline comment in a private local draft. Press **Ctrl-Shift-X** from the full diff to finish: abandon
locally, submit a comment-only review, approve, or request changes. Each submission
sends the summary and all saved inline comments in one GitHub review. The `ready`
command also marks the checked-out PR ready for review and can request a reviewer.
This uses Zed tasks and terminal UIs, not a GUI extension.

## Setup

Install Rust **1.89 or newer** with Cargo, `jj`, `git`, `gh`, and the `zed` CLI.
Use **jj 0.47.1 (tested)**; the live log needs a recent jj with JSON template
metadata support. Viddy is not required. These commands must be on the PATH
available to Zed tasks. Authenticate `gh` as the reviewer, using the appropriate
`--hostname` for GitHub Enterprise:

```sh
gh auth login
gh auth status
```

Install this dotfiles configuration at `~/.config/zed`, then build the local
binary:

```sh
cargo build --locked --release --bin zed-pr-review --manifest-path ~/.config/zed/zed-pr-review/Cargo.toml
```

The tasks in `../tasks.json` run
`~/.config/zed/zed-pr-review/target/release/zed-pr-review`. Rebuild after changing
the Rust source, including when upgrading to the live log viewer or finish-review
dialog; the tasks do not build automatically. Open a new diff after upgrading to
get cached patch metadata for offline commenting; older tabs keep their existing
live GitHub checks.

Open the jj workspace in Zed, press **Ctrl-Shift-G**, and enter a PR number or
URL. You do not need to fetch or track its original branch first. `open` fetches
the PR and base history, preserves your work, and starts a review session.
For mutable heads it runs `jj edit <head>`; for immutable heads it runs
`jj new <head>`, leaving the PR commit unchanged. See the safeguards below.

You can also prepare the checkout yourself with `jj git fetch` and `jj edit`
(or `jj new` for an immutable head), then run `PR: full diff` before commenting
or submitting. `open` and review navigation (`sb`/`sf`) fetch and switch revisions;
the other review commands do not. The recorded jj `@` must be either the **exact
PR head** or a **conflict-free, empty child with that head as its only parent**.
The child's complete tree must match the head; merges, grandchildren, and children
with recorded file changes are rejected. Manually created empty children also
qualify. These checks use recorded jj state, not unsaved or unsnapshotted edits.
Comments and comparisons remain pinned to the PR head, never the child. Comments
need local head blobs; `diff` also needs the pinned base and head commits and
enough history to find one merge base.

An existing review session supplies the PR for commands without `--pr`, so its
original branch bookmark is not required. Without a session or explicit `--pr`,
automatic discovery requires one unconflicted local bookmark and one matching
open PR. It checks `@` first. Only if there are no non-tool bookmarks there may it
check the sole parent of an empty, conflict-free `@`. It never falls back after
ambiguous or failed discovery, or searches further ancestors. Tool bookmarks under
`zed-pr-review/` are excluded. Use `--pr` and, when needed, `--repo` if discovery
is ambiguous; these flags do not relax the checkout check. **`comment`,
`submit-comment`, `approve`, and `request-changes` always require context from a
live full-PR diff**, even with an explicit `--pr`. They do not fall back to
bookmark discovery or the last active session. **`finish` and `cancel` also require
an exact registered full-diff path**, on either the head or base side, and reject
`--draft-dir`, `--pr`, and `--repo`. Local cancellation can work offline after the
checkout or remote PR changes; submission still requires the pinned checkout
and remote revision.

## Zed tasks and shortcuts

| Task                  | Shortcut     | Purpose                                                                |
| --------------------- | ------------ | ---------------------------------------------------------------------- |
| `PR: open for review` | Ctrl-Shift-G | Fetch and check out a PR, then open its full comparison.               |
| `PR: add comment`     | Ctrl-Shift-C | Read an inline comment in a terminal and save it locally.              |
| `PR: finish review`   | Ctrl-Shift-X | Choose local abandonment, a comment-only review, approval, or changes. |
| `PR: list comments`   | None         | Show the draft, its comments, and recovery status.                     |
| `PR: full diff`       | None         | Rebuild the full comparison for the checked-out PR.                    |
| `jj l`                | Ctrl-Shift-L | Show the PR stack in review mode; otherwise show the live jj log.      |
| `jj sb`               | Ctrl-Shift-J | Open the parent PR in review mode; otherwise run `jj sb`.              |
| `jj sf`               | Ctrl-Shift-K | Open a child PR in review mode; otherwise run `jj sf`.                 |

Ctrl-Shift-G is global. Ctrl-Shift-C and Ctrl-Shift-X have `Editor` context,
not terminal context. Ctrl-Shift-C adds an inline comment; Ctrl-Shift-X opens
the finish dialog in the center pane. Use Zed's `task: spawn` picker for any
named task. The old Ctrl-Shift-A/Q mappings are removed. The finish task replaces
the old approve/request-changes tasks; CLI `approve` and `request-changes` remain
available. Ctrl-Shift-G and Ctrl-Shift-J/K/L are unchanged.

Every `PR:` task runs with `cwd: $ZED_WORKTREE_ROOT`, `reveal: always`,
`allow_concurrent_runs: false`, and `use_new_terminal: true`. The finish task uses
`hide: on_success`; the others use `hide: never`. A new terminal avoids reusing a
terminal that is waiting for comment input.

**`PR: open for review` uses `save: all`** before changing the checkout. The other
`PR:` tasks use `save: none`. Open and finish run with `/bin/zsh -f`. The CLI also
holds a workspace lock for review operations and a separate lock for each draft,
including while waiting for input or an editor. These locks do not prevent
unrelated jj commands or file edits; do not change the workspace concurrently.

### Open a PR for review

`PR: open for review` runs the binary's `open` command. Without the global `--pr`
flag, it prompts for a PR number or URL and reads **one stdin line**. Press Enter
to continue; a blank line cancels without repository changes. Unlike comment
input, this does not require Ctrl-D. For example:

```sh
~/.config/zed/zed-pr-review/target/release/zed-pr-review open
~/.config/zed/zed-pr-review/target/release/zed-pr-review --pr 123 --repo owner/repo open
~/.config/zed/zed-pr-review/target/release/zed-pr-review --pr https://github.com/owner/repo/pull/123 open
```

`open` performs these steps:

1. Resolve the PR and authenticated reviewer. Fetch `refs/pull/N/head` and the
   base branch's history from the **base repository**, including for fork PRs.
   The fetch uses HTTPS and `gh` credentials, rejects URL rewrites and redirects,
   and obtains full history when the repository is shallow. The fetched head must
   equal the pinned PR head. GitHub's PR base SHA can lag behind the base branch:
   a newer fetched tip is accepted only when it contains that pinned base commit.
   The diff still uses the original pinned base, not the newer tip. Recheck the
   PR's pins and base branch before import. If exactly one configured remote matches
   the PR's **source host and repository**, also fetch its source branch, verify
   its head, and run `jj bookmark track` as needed.
2. Prepare frozen snapshots for `merge-base(base, head) → head` in the default
   repository-local state directory.
3. Snapshot the current jj working copy and record its full commit SHA, without
   creating a bookmark. Then run `jj edit <head-SHA>` for a mutable head or
   `jj new <head-SHA>` for an immutable head, and verify the resulting checkout.
   Neither command overrides immutability or changes jj configuration.
4. Save `last-checkout.json` in the per-PR draft directory, activate the session,
   print the previous SHA in a `jj edit <previous-SHA>` return command (use
   `jj new <previous-SHA>` if it is immutable), and open the comparison in Zed.
   If checkout succeeds but Zed cannot open the view, run
   `PR: full diff` rather than repeating checkout just to reopen it.

Save buffers yourself before using `open` directly; only the Zed task saves them
for you. The preserved commit contains the work recorded by jj, not unsaved
editor buffers or ignored files.

The `zed-pr-review/` namespace is reserved for tool refs and bookmarks. Fetches
still keep unique refs under `refs/heads/zed-pr-review/`, but checkout and stack
navigation do not create `previous-*` bookmarks. Existing bookmarks are left
untouched. Tool refs and bookmarks are excluded from automatic PR discovery.

Return to your starting revision with the printed command or the `previous_commit`
SHA in `last-checkout.json`. jj may hide an empty, undescribed revision after you
leave it; the full SHA still identifies it. This uses normal jj history rather
than a permanent bookmark, so it is not an indefinite archive against later
history pruning or garbage collection.

`open` refuses sparse or stale workspaces, conflicted current or target
revisions, submodule checkouts, unsafe paths, and untracked
or ignored files that obstruct checkout. Before switching, it also checks whether
the target's ignore rules would expose existing ignored files, such as `.env`,
and refuses that checkout without recording those files. Directories that remain
ignored, such as `node_modules`, are left alone. Final verification disables
automatic tracking of new files. It also refuses reserved state paths in the
current revision, Git index, or incoming head. It never pushes, resets,
rebases, or automatically rolls back. A failure may leave fetched refs or
snapshots; after checkout, it may leave `@` at the PR head or its new child.
Inspect the reported state and use the printed return command rather than
assuming that failure restored the old checkout. If a fetch fails before import,
a later attempt may report pending Git ref changes. Run `jj git import` and
inspect the imported refs before retrying; the review tool does not silently
import unrelated pending changes.

### Fetching and tracking branches

There is no need to run `jj git fetch` or `jj b t` before **Ctrl-Shift-G** or a
move through the PR stack. The CLI uses a scoped, hardened HTTPS Git fetch plus
`jj git import`, rather than a broad `jj git fetch`. This avoids unrelated
refspecs, configured transport helpers, and automatic bookmark tracking. It
still uses `gh` credentials, including when the matching remote has an SSH URL.

When one configured remote matches the source repository, the CLI fetches only
the PR's branch, verifies it against the PR head, and tracks that remote
bookmark. It refuses conflicting local bookmarks, non-fast-forward updates,
locally deleted bookmarks, mismatched branch heads, or ambiguous matching
remotes. It never creates a remote or overwrites conflicting work. Fork PRs
without a matching source remote still work through the base repository's
`refs/pull/N/head`; they do not need a source bookmark.

Because jj imports Git refs globally, the CLI also refuses unrelated pending
Git branch or tag changes before importing. Otherwise, an already-tracked
bookmark could move or disappear during review setup. Resolve those changes
manually and retry. A late refusal can leave verified fetched refs in Git; it
does not import the unrelated changes or roll them back. Do not run other
repository operations concurrently.

### Show and navigate the PR stack

**Ctrl-Shift-L** opens a live PR stack in the center pane. The graph reads like
`jj log`: children appear above parents, `@` marks the current PR, and `○` marks
other PRs. Linear stacks stay in one column rather than gaining indentation.
Forks use extra graph lanes only where needed. Each PR shows its number, branch,
draft/ready status, title, and URL, with colors for metadata and dim connectors.
The base branch appears once at the bottom.

The view stays open until **q**, **Esc**, or **Ctrl-C**. GitHub reads run in the
background every **30 seconds**, rather than every second like the local jj log.
Press **r** to refresh immediately. Use **j/k**, arrow keys, the wheel, or
PageUp/PageDown to scroll; **h/l** pans horizontally, and **@** reveals the current
PR. Home/End and Ctrl-U/Ctrl-D also work. Shift-drag selects text. Refresh failures
leave the last graph visible and show an error; a later successful read clears it.
The viewer does not change the checkout or hold the workspace lock, so other
review tasks can run while it stays open. It also starts background preparation
and shows its progress, as described below.

After validating the originating review, the viewer follows the active PR on
refresh when you move through the stack. An explicit `--pr` keeps it pinned to
that PR instead. Every read still checks the review's pins and checkout; the
viewer never rebinds an old diff tab or changes the checkout.

**Ctrl-Shift-J** opens the parent PR; **Ctrl-Shift-K** opens a child PR. Each move
uses the same fetch, tracking, preservation, checkout checks, and full diff
as **Ctrl-Shift-G**. With several children, enter one of the offered **PR
numbers**, then press Enter. A blank line cancels. At either end of the stack,
there is no checkout. Saved comments remain in their separate per-PR drafts;
returning to an unchanged PR does not discard them.

Stack links come from GitHub's exact repository and branch relationships:
`parent.head repository/ref == child.base repository/ref`. The parent's head
must also match the child's base SHA. Neither PR numbering, branch-name
prefixes, PR descriptions, nor commit ancestry defines a stack. Unrelated PRs
that all target `main` are not grouped together. Closed or merged PRs are not
included. Cycles, ambiguous parents, and observed changes to the stack cause a
refusal rather than a guessed checkout. If the offered children change during
navigation, retry to see the new choices.

The three shortcuts decide their mode from Zed's task context:

- **Registered diff snapshot, either side:** review mode. The snapshot and the
  checkout must still match the pinned PR revision. An old or altered tab fails closed;
  it never falls back to jj navigation or binds itself to a newer session.
- **Ordinary source file:** normal jj mode, even if a review session exists.
  J and K run the configured `jj sb` and `jj sf` aliases. L opens the Rust live
  log viewer described below, not viddy.
- **No file, such as a terminal:** use the active review only while `@` equals
  its pinned head or qualifies as its empty child. Otherwise, use normal jj mode. A corrupt active pointer is
  an error, not permission to change revisions.

Navigation from either diff side does not enable deleted-side comments:
**`PR: add comment` still requires a right-side selection**. J and K save buffers
before running, as the previous jj tasks did. They share the workspace lock
with comment input and review submission. For direct CLI use, save buffers
first. Run `stack` for a one-time PR stack or local jj log. `stack --watch`
enables the corresponding live view only when **both stdin and stdout are
terminals**. If either is redirected or piped, it prints the PR stack or runs
`jj l` once instead. PR output uses colors on a terminal and plain text when
stdout is redirected. `--pr` cannot force a source buffer into review mode;
use `open` first.

### Background preparation and quick stack navigation

After **Ctrl-Shift-G** opens the current diff, a detached worker prepares the rest
of its stack. **Ctrl-Shift-L** also starts preparation after loading the graph.
The worker visits the nearest child, then the parent, then works outward in
breadth-first order. Forks are included. One worker runs per workspace, with one
PR prepared at a time to limit disk and network contention.

Preparation fetches Git objects into `refs/zed-pr-review-prefetch/` and writes
frozen diff snapshots plus cached GitHub patches. It **does not change `@`, import
Git refs into jj, track bookmarks, activate a session, open tabs, or change drafts**.
No workspace lock is held. Network reads happen outside per-PR draft locks and
have a 60-second deadline per command. Foreground review commands have priority;
if you jump to a PR while its local snapshots are being written, navigation waits
up to 30 seconds for that work rather than preparing it twice.

Ctrl-Shift-J/K reuse prepared snapshots and fetched objects when their exact PR
head, base, source, and reviewer still match. Changed revisions use fresh data;
cleaned or cancelled snapshots are never reused. Returning to an unchanged PR
can reopen the same frozen diff. Explicit `open` and `diff` still create new
snapshot directories, so old tabs are never assigned to another revision.

Navigation also reuses a graph for up to **60 seconds**, while checking the
current and target PRs live. New children can appear on the next full refresh:
press **r** in the stack view or run `stack`. The live view refreshes every 30
seconds. It shows `Prepared N/M adjacent PRs`; failures are recorded in
`scratch/zed-pr-review/prefetch/status.json` and do not prevent ordinary opening.

This makes additional authenticated GitHub reads and Git fetches, and uses local
disk space; it never posts reviews or pushes. Prepared snapshots follow normal
review cleanup. Fetched refs are retained, like the existing review refs.
Set `ZED_PR_REVIEW_PREFETCH=0` in the Zed task environment to disable automatic
preparation. A worker already running finishes its current finite pass.

### Use the live jj log

In normal jj mode, **Ctrl-Shift-L** runs `stack --watch` in a Rust terminal UI.
It refreshes once per second using the configured `jj l` graph and colors, with
metadata added through template aliases for this invocation only. It does not
change your jj configuration or contact GitHub. There is no fallback to viddy
and no use of system URL handlers for clicks.

| Input                       | Action                               |
| --------------------------- | ------------------------------------ |
| Click a mutable change ID   | Run `jj edit` for that revision.     |
| Wheel, `j` / `k`, Up / Down | Scroll vertically.                   |
| `h` / `l`, Left / Right     | Scroll horizontally.                 |
| PageUp / PageDown           | Scroll by a page.                    |
| Ctrl-U / Ctrl-D             | Scroll by half a page.               |
| Home / End, `g` / `G`       | Go to the top / bottom.              |
| Tab / Shift-Tab             | Select the next / previous revision. |
| Enter                       | Edit the selected revision.          |
| `@`                         | Reveal the working-copy revision.    |
| `r`                         | Refresh now.                         |
| `q`, Esc, Ctrl-C            | Exit.                                |

A mouse click requires a press and release on the same ID without dragging.
Immutable and divergent revisions appear inactive and cannot be edited. Before
an edit, the CLI rechecks the target and refuses stale or divergent revisions.
It respects jj's immutable rules and never passes `--ignore-immutable`. Edits
use normal jj snapshotting and take the workspace lock if review state already
exists. They do not create a review session. The lock does not exclude unrelated
jj commands or file edits; avoid concurrent workspace changes. An edit can
snapshot local work even if a later check rejects the target.

Refreshes keep the same revision at the top when possible, rather than just the
same row number. The viewer reveals `@` initially and when its change identity
moves, not on ordinary refreshes or content-only snapshots of that change.
Press `@` to return after scrolling away. Resizing preserves your position when
possible.

**Saving and polling:** the `jj l` task uses `save: all` on startup, not on each
click. Zed's `autosave: on_focus_change` still matters when moving from an editor
to the running viewer; the CLI cannot save all buffers on every edit. Save
buffers yourself before using the CLI directly. Polls use
`--ignore-working-copy --at-operation=@`: they neither snapshot files nor merge
concurrent operations. Unsaved buffers and file changes that jj has not yet
snapshotted do not appear in the log. Even `r` only rereads recorded state;
another normal jj command must record those changes.

**Limits:** custom templates that bypass the formatter aliases can still print
IDs, but those IDs have no clickable metadata and cannot be selected for edits.
The viewer has no viddy history, search, or diff view.

### Mark a PR ready

The existing **Ctrl-Shift-R** shortcut and `jj revision ready` task now run the
Rust `ready` command. Their other task settings are unchanged.

```sh
~/.config/zed/zed-pr-review/target/release/zed-pr-review ready
~/.config/zed/zed-pr-review/target/release/zed-pr-review ready --reviewer alice
~/.config/zed/zed-pr-review/target/release/zed-pr-review --pr 123 --repo owner/repo ready
```

The command first captures the recorded **jj `@`**, before contacting GitHub.
That commit must equal the current PR head or qualify as its empty child,
including when `--pr` is explicit.
Discovery uses the captured commit, and all later checks and writes use the same
PR. You can move to another revision while `ready` runs; it will finish on the
original PR without changing your new checkout.

An existing session can supply the PR without its original bookmark. Otherwise,
automatic discovery checks bookmarks at the captured commit, then at its sole
parent only if the captured commit is empty, conflict-free, and has no non-tool
bookmarks. It never searches further ancestors. Explicit `--pr` can select the PR
without a bookmark but cannot bypass the initial checkout check.

`ready` marks a draft PR ready; an already-ready PR does not need that mutation.
An optional `--reviewer LOGIN` requests one user. Use `--reviewer org/team-slug`
for a team in the base repository's organization. Without the flag,
`ZED_REVISION_READY_REVIEWER` supplies the reviewer; an unset or whitespace-only
value means no request. The flag takes precedence, and an explicitly blank flag
is an error. Reviewer requests add to existing requests rather than replacing
the reviewer list. Repeating `ready --reviewer ...` may notify the reviewer again.

The command rechecks the remote PR head, base commit and branch, identity, and
authenticated account before and after each mutation. It does not recheck the
live jj `@`. A remote change detected after marking ready stops the command
before it requests a reviewer. A failed
request reports that the PR was already ready and that the request may have
succeeded despite the error. No mutation is automatically retried or rolled back;
check GitHub before trying again.

GitHub's mark-ready API has no atomic expected-head condition. Another push can
race with the final check; post-action checks report a detected change but cannot
undo a notification. The initial capture uses the recorded revision, not unsaved
Zed buffers or file edits that jj has not recorded. Those edits are not included
in the PR by this command.

`ready` never pushes, fetches, or snapshots the working copy. It does not open an
editor or read, lock, create, or alter a private review draft, so an existing
review or uncertain submission does not block it. Normally it creates no private
state. It can read an existing local session association and validate that state;
`--state-dir` or `ZED_PR_REVIEW_STATE_DIR` affects whether that association is
used. The initial checkout and remote before/after mutation checks still apply. `--editor`
has no effect; `--draft-dir` is rejected.

### Add a comment

1. Use a registered **head snapshot** from `open` or `diff`. Ordinary source
   files are not review context, even when a saved session matches `@`. Keep the
   snapshot unchanged on disk and in the editor; the comment task does not save
   buffers. The CLI checks the whole file against its pinned snapshot digest. Bare-CR files are rejected because their editor rows can
   differ from GitHub's line numbers.
2. Select one continuous, nonblank range on the **RIGHT (head) side**, within one
   excerpt and one GitHub diff hunk. Added lines and right-side context are
   supported; deleted-side comments are not.
3. Press **Ctrl-Shift-C**, or run `PR: add comment` from Zed's `task: spawn`
   picker. Check the PR, path, and line range shown in the terminal.
4. Type a multiline comment. Press Enter, then **Ctrl-D on an empty line** to
   finish stdin. A nonblank body is saved locally; nothing is published. A blank
   body cancels. Ctrl-C cancels the current input and keeps earlier comments.

**New sessions save comments entirely locally.** The prompt and save use the
frozen head bytes and cached GitHub patch; they make no GitHub requests or Git
blob reads, and do not scan all tracked repository paths. The recorded jj
checkout must still match the pinned PR, and the session and selection must be
intact. A remote push or account change does not prevent saving a private draft:
submission checks GitHub again and refuses mismatched revisions or accounts.
Legacy sessions without cached patches use the previous live checks. Open a new
diff to enable the local path; existing drafts are preserved.

The selection behavior was verified in the **Zed 1.29.3 source**, whose relevant
code is unchanged from 1.29.2; the actual GUI workflow has **not** been tested.
Zed supplies `ZED_FILE`, `ZED_ROW`, `ZED_COLUMN`, and `ZED_SELECTED_TEXT` through
the task environment. Rows and UTF-8 byte columns are one-based and refer to the
selection start, even for a reverse selection. Zed exports only the newest
selection, with no reliable diff-side metadata. The CLI cannot reliably detect
multiple selections or determine which side you selected. **A reported RHS path
does not prove that the selection came from the RHS.** Use exactly one nonblank,
continuous right-side selection within one excerpt and one hunk.

For snapshots, the CLI maps only the exact registered `head/` path in that
session's manifest to its repository filename. It validates the manifest and
snapshot digests, the selected text at the supplied coordinates, and the range
against the cached GitHub patch. It never
finds a file or line by searching for matching text. Base snapshots, unknown
paths, modified snapshots, cleaned sessions, cancelled sessions, and a mismatched
recorded checkout are rejected.

The CLI reads these environment variables directly. **Never interpolate selected
text into a task command, shell argument, or shell script**, even inside quotes.
Do not add an `echo` or other shell wrapper to pass it to the CLI. The terminal's
stdin is for the comment body, not the selected source text.

### Review the full PR

Zed's project diff compares jj `@` with its parent and may omit earlier commits
in a multi-commit PR. Both `open` and `PR: full diff` (CLI `diff`) compare
**`merge-base(base, head) → head`** with `zed --existing --diff`.

`open` fetches and switches the checkout. `diff` instead reads existing local
Git objects and builds a new session for the pinned PR head, with `@` at that
head or its empty child. It does not fetch, switch revisions, or snapshot the
working copy. It still contacts
GitHub to check the PR and obtain its changed-file metadata.

Each comparison has a unique directory:

```text
REPO_ROOT/scratch/zed-pr-review/<draft-key>/diff-<unique>/
  base/
  head/
  manifest.json
  cleanup.json    # added after a confirmed submission, before removing snapshots
  cancelled.json  # added for local abandonment instead; retained after cleanup
```

The snapshots are frozen copies of the changed files, not live workspace files.
The manifest records the pinned PR, merge base, repository paths, sides, and
content digests and GitHub patches. `active.json` at the state root identifies the
active session; `<draft-key>/prepared.json` identifies its reusable prepared diff.
Explicit `open` and `diff` create new directories rather than replacing old
snapshots; stack navigation can reuse an unchanged prepared directory.
**Old tabs stay bound to their original session**, even after another PR opens.
You can add inline comments from registered head snapshots with `PR: add comment`;
do not edit the snapshots. Cancelled tabs are rejected rather than rebound to a
new session.

**`comment`, `submit-comment`, `approve`, and `request-changes` require an exact
registered snapshot from a live session.** The CLI refuses ordinary source files,
missing file context, prompt/metadata files, and cleaned or cancelled sessions
before creating state, contacting GitHub, reading comment input, or opening the
summary editor. A saved active session, matching bookmark, or explicit `--pr`
cannot bypass this. Inline comments require the right side; either side can
provide context for submission.

`finish` and `cancel` also require the exact registered head/base path through
`ZED_FILE`; an ordinary source file or active pointer alone is not enough. They
reject `--draft-dir`, `--pr`, and `--repo` rather than letting those flags select
another review. Local cancellation does not require a matching checkout or an
unchanged remote PR. An interrupted cancellation can be retried from its old
exact tab as described below; this does not permit submission from cancelled tabs.

Other commands, such as listing and recovery, can still use the active session
from ordinary files. No session-based command needs the original PR branch.
An explicit `--pr` must agree with the session; it cannot rebind an existing view
or active review.
For commands that check the current revision, a mismatch in `@`, the PR head,
base, or identity causes a refusal rather than silently choosing a different PR.
`@` must match the pinned head or its empty child. Local comments use pinned data
and defer remote checks until submission. Local cancellation does not require
either live remote checks or a matching checkout.
Run `open` again to switch or refresh the active review. Reopening does not
reassign old comments to a new revision; see the draft guidance below.

Renames may appear as removal/addition pairs. Pure renames, mode-only changes,
binary files, and files without a GitHub patch may have no useful text diff.
The CLI prints file status and rename information, plus binary/no-patch caveats.
Binary and no-patch files are view-only; deleted files have no RHS for comments.

### Finish a review

Use `PR: list comments` to check the draft, then press **Ctrl-Shift-X** from either
side of the registered full-PR diff. `PR: finish review` runs CLI `finish` and
opens a **Ratatui dialog in the terminal**, not a native Zed dialog.

| Choice                  | Action                                                                                                                             |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| **1 — Exit + abandon**  | Archive and clear the local draft, remove its review snapshots, and clear only a matching active pointer. No GitHub or jj changes. |
| **2 — Comment**         | Submit a `COMMENT`-only review with a summary and all saved inline comments. Allowed on your own PR.                               |
| **3 — Approve**         | Submit an `APPROVE` review with a summary and all saved inline comments.                                                           |
| **4 — Request changes** | Submit a `REQUEST_CHANGES` review with a summary and all saved inline comments.                                                    |

Press **1–4** to choose immediately, or use **Up/Down, then Enter**. Nothing is
selected initially. **Esc, q, Ctrl-C, or Enter with no selection** dismisses the
dialog without changing the draft or snapshots. Dismissal is not choice 1.
If either stdin or stdout is not a TTY, `finish` prints numbered choices and
reads **one line**: enter `1`–`4`, then Enter. Blank input or EOF dismisses it;
invalid input fails without choosing an action. No Ctrl-D is needed after a
numbered line.

#### Exit and abandon locally

Choice **1** performs local cancellation immediately. CLI `cancel` does the same
after asking for the literal `yes`; `cancel --yes` skips that prompt. Both require
the exact registered head/base context, just like `finish`. Cancellation works
**offline**, even if you changed the checkout or the remote PR changed.

Cancellation archives the draft, clears `draft.json`, and removes the `head/`
and `base/` snapshots from its saved sessions for that PR and reviewer. It removes
`active.json` **only if it points to that same PR/reviewer draft directory**.
Other reviews and their active pointer remain untouched. It retains archives,
session manifests, `cancelled.json` markers, and the `last-checkout.json` return
receipt. This is not deletion of the whole state directory.

**No GitHub review is posted or deleted, no `jj abandon` is run, and the checkout
stays as it is.** The CLI cannot close Zed tabs; old diff tabs may remain
visible even though their files are gone. To return manually, use the earlier
printed command or the `previous_commit` SHA in `last-checkout.json` with
`jj edit <previous-SHA>` (or `jj new <previous-SHA>` if immutable). Cancellation
does not run that command or create a return bookmark.

Cancellation refuses a submitted draft or any uncertain submission attempt or
retry history. Use `recover` to resolve submission state or retry submitted
cleanup; cancellation must not erase evidence of a possible GitHub review.
A submitted draft still requires explicit `discard` before another review.

After local cancellation, `open` or `diff` can create a fresh review session.
Old cancelled tabs cannot add comments, submit, navigate, or bind themselves to
the new session. If cancellation stops partway through, fix the reported problem
and retry **`cancel --yes`**, or **`finish` → 1**, from the **old exact tab**.
That retry remains available only until a new session is created; it cannot
cancel a replacement review. Altered or unregistered files are refused rather
than deleted.

#### Write a summary and submit

Choices **2–4** use the existing summary editor. `PR: finish review` explicitly
passes `--editor 'zed --existing --wait'`, so the shell's `EDITOR=vim` does not take
over. Choice 2 publishes a comment-only review; it does **not** create a local
inline comment. Use `PR: add comment` or CLI `comment` for that before finishing.

The CLI opens a Markdown message file with room for your summary at the top.
Below it, each saved comment includes its file and line range, the commented
lines with up to **five lines before and after**, and your comment underneath.
The source comes from the pinned PR head, not the working copy. A `>` marks each
commented line; the other numbered lines provide context.

Every reference line starts with `#|`. These lines are removed from the summary
before submission, even if indented. Ordinary Markdown headings such as
`# Summary` are preserved. The reference section is only for viewing: editing
its copy of a comment does not change the saved comment. Use `edit INDEX` for
that. With no saved comments, the message file is empty.

**`zed --wait` waits until the message tab closes, not until it is saved.** Save a
nonblank summary and close that tab to submit. Closing a tab with a saved
nonblank summary **will submit** after the safety checks pass; closing is not a
cancel action, and there is no extra confirmation prompt.

To stop without submitting, **clear the summary, save, then close the tab**. You
can leave the `#|` reference lines in place. An empty or whitespace-only summary
after removing those lines aborts any of choices 2–4, even with saved inline
comments, and preserves the draft and snapshots. This is not local abandonment.
Alternatively, Ctrl-C in the task terminal cancels the editor process;
the local draft persists. If submission may already have started, follow recovery
below instead of assuming cancellation prevented the POST.

#### Cleanup after submission

After GitHub confirms a comment-only review, approval, or request-changes review
and the CLI saves its receipt, it **removes the `head/` and `base/` snapshots** from
every saved session for that PR and reviewer, including earlier opens and revisions. Other
PRs and reviewers are untouched. The CLI keeps `draft.json` with your comments
and receipt, message files, archives, and the small session manifests. Submitted
cleanup accepts `COMMENTED` receipts as well as `APPROVED`, `CHANGES_REQUESTED`,
and recovered `DISMISSED` receipts.

Each cleaned session also keeps a `cleanup.json` marker. Together with
`active.json`, this preserves the PR identity so **Ctrl-Shift-L/J/K still work
from the reviewed tab or task terminal**. The old tab may remain open in Zed,
but its source files are gone and cannot accept new comments. Use `PR: full diff`
or reopen the PR to create fresh snapshots. Starting another review still
requires explicitly discarding the submitted draft.

Dismissing the finish dialog, leaving a blank summary, interrupting the editor,
failed submissions, uncertain responses, and failures to save a receipt **do not
remove snapshots**. Explicit local abandonment (choice 1 or `cancel`) does.
A successful `recover` cleans them once it confirms the submission and saves the
receipt, including when you use
`--draft-dir` from another directory. Cleanup needs access to the original jj
workspace; old external state directories are not modified.

If cleanup fails, the CLI prints a warning without turning a successful review
into a failed submission. Comments and the saved receipt remain intact. Fix the
reported filesystem or workspace problem, then run `recover` with `--draft-dir`
pointing to the saved draft directory to retry cleanup only. **Do not submit the
review again.** Interrupted cleanup can resume; altered or unregistered files
are refused rather than deleted.

## CLI reference

Run the binary directly for actions without tasks or for explicit PR selection:

```sh
~/.config/zed/zed-pr-review/target/release/zed-pr-review --help
~/.config/zed/zed-pr-review/target/release/zed-pr-review --pr 123 --repo owner/repo open
~/.config/zed/zed-pr-review/target/release/zed-pr-review --pr 123 --repo owner/repo list
~/.config/zed/zed-pr-review/target/release/zed-pr-review edit 1 --editor 'zed --existing --wait'
```

The PR number and repository above are examples. `comment` normally runs through
Zed because it requires the selection variables from the task environment.
`finish` and `cancel` require `ZED_FILE` to identify an exact registered full-diff
head/base path. Running from the workspace directory alone is not enough, and
`--draft-dir`, `--pr`, and `--repo` are rejected for both commands.

| Subcommand        | Behavior                                                                                                    |
| ----------------- | ----------------------------------------------------------------------------------------------------------- |
| `open`            | Fetch, track when safe, preserve local work, check out the PR, and open a session.                          |
| `stack`           | Show the GitHub stack in review context; otherwise run `jj l` once.                                         |
| `stack --watch`   | Live PR stack or local jj log when stdin/stdout are terminals; otherwise the corresponding one-shot output. |
| `sb` / `sf`       | Open the parent/child PR in review context; otherwise run the corresponding jj alias.                       |
| `ready`           | Mark the selected PR ready from its head or empty child; optionally use `--reviewer`.                       |
| `comment`         | Read a multiline body from stdin until EOF; save a local inline comment.                                    |
| `finish`          | Choose local abandonment, a comment-only review, approval, or changes from the selected full diff.          |
| `cancel`          | Ask for `yes`, then archive and clear the local draft and snapshots; leave GitHub and jj unchanged.         |
| `cancel --yes`    | Perform the same local cancellation without prompting; still require exact full-diff context.               |
| `submit-comment`  | Edit a nonblank summary; send one `COMMENT`-only review with all saved comments, including on your own PR.  |
| `approve`         | Edit a nonblank summary; send one approval with all saved comments.                                         |
| `request-changes` | Edit a nonblank summary; send one request for changes with all saved comments.                              |
| `list`            | Print one-based comment indices, pinned revision, status, and draft directory.                              |
| `edit INDEX`      | Edit a saved comment body; a blank edit preserves the original.                                             |
| `remove INDEX`    | Remove a saved comment locally.                                                                             |
| `discard`         | Ask for `yes`, archive the draft, and clear it locally.                                                     |
| `discard --yes`   | Archive and clear without prompting; never delete a GitHub review.                                          |
| `recover`         | Reconcile an uncertain submission or retry snapshot cleanup; never send another review.                     |
| `recover --retry` | If safe to proceed, ask for `yes` to permit a later attempt; do not post.                                   |
| `diff`            | Build a frozen full-PR session for the checked-out PR, without fetching or switching.                       |

Indices are one-based and refer to the current `list` output. All of these flags
are global:

| Flag                       | Meaning                                                                                                                                                                                           |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--directory PATH`         | Workspace directory; otherwise prefer the selected `ZED_FILE`'s jj workspace, then `ZED_WORKTREE_ROOT`, then the current directory. `--draft-dir` skips the selected-file hint.                   |
| `--pr NUMBER_OR_URL`       | Explicit PR number or `https://HOST/owner/repo/pull/NUMBER`. Rejected by `finish` and `cancel`.                                                                                                   |
| `--repo [HOST/]owner/repo` | Base repository, including an optional GitHub Enterprise host. Without a host, uses `github.com` unless a PR URL supplies one. Rejected by `finish` and `cancel`.                                 |
| `--state-dir PATH`         | State root; overrides `ZED_PR_REVIEW_STATE_DIR`. Defaults to `REPO_ROOT/scratch/zed-pr-review`. External paths remain supported for draft commands, not review sessions or PR stack navigation.   |
| `--draft-dir PATH`         | Open a specific draft directory without PR discovery; only for `list`, `edit`, `remove`, `discard`, and `recover`. Rejected by `finish` and `cancel`. Cannot be combined with `--pr` or `--repo`. |
| `--editor COMMAND`         | Editor command and arguments, parsed without shell evaluation.                                                                                                                                    |

Editor precedence is `--editor`, `ZED_PR_REVIEW_EDITOR`, `EDITOR`, then `vim`.
When using Zed, include `--wait`; `zed --existing --wait` is recommended.

## Private drafts and changed checkouts

The default state root is now **`REPO_ROOT/scratch/zed-pr-review`**, where
`REPO_ROOT` is the jj workspace root. Each draft has a separate `<draft-key>`
directory based on host, repository, PR number, and reviewer. Alongside the
drafts and sessions, the state root contains `active.json` and the `workspace/`
lock directory. `last-checkout.json` lives in the per-PR draft directory.

Before writing private state, the CLI checks for reserved paths in jj `@`, the
Git index, and the incoming head when available. It creates an untracked
`scratch/zed-pr-review/.gitignore` containing `*`, which ignores itself and all
state contents in Git and jj, then verifies the ignore protection. It never
edits a tracked `.gitignore` or untracks files. Conflicting ignore contents,
tracked reserved paths, symlinks, hard-linked state files, and other unsafe
filesystem entries cause an error rather than being replaced.

Draft directories have private Unix permissions. Drafts are local, not pending
GitHub reviews. Messages, archives, and snapshots may contain private source or
review text; keep the state out of source control and shared storage. **This is
not a disposable cache: deleting the state loses saved comments and submission
recovery records**, not just snapshots. Successful reviews remove their source
snapshots automatically, not the saved comments or receipts. Local cancellation
also removes snapshots, but archives and clears the draft while retaining its
cancellation markers, manifests, and return receipt.

`ZED_PR_REVIEW_STATE_DIR` or `--state-dir` can still select an external state root
for draft commands. The default path is the only allowed location inside the
reviewed repository. Full sessions (`open`, `diff`, comments from snapshots,
`finish`, and `cancel`) require that default repository-local state; an external
override produces a clear error. Omit `--state-dir` and unset `ZED_PR_REVIEW_STATE_DIR` for that
workflow, after checking whether you have an older draft to preserve.

The CLI prints the exact per-PR draft directory. Pass that path to `--draft-dir`,
not the parent state directory. With this flag, `list`, `edit`, `remove`, and
`discard` work offline, independently of jj, local Git objects, and the current
checkout. `recover` also works independently of the checkout, but **requires
GitHub access** and the original authenticated reviewer. Use an existing
`--directory` if `ZED_WORKTREE_ROOT` points to a directory that no longer exists.
Commenting and submission require both a live review snapshot and a recorded
`@` that matches the pinned head or its empty child. Marking ready uses the same
checkout rule, but it can use a session, explicit flags, or bookmark discovery;
it does not use the local draft. Unlike submission, local cancellation from the
exact registered diff can work offline after checkout or remote changes; it does
not accept `--draft-dir`.

If the PR head, base, or identity changes, the CLI keeps existing comments and
refuses to reuse them for a new revision. Inspect the old draft, explicitly
`discard` it when appropriate, and review the new revision. To abandon the local
review and remove its snapshots as well, use `cancel` from its exact diff context.
A submitted draft stays blocked until an explicit `discard` starts a new review.
Discard keeps an archive and never changes GitHub; it is refused while submission
is uncertain. Local cancellation also refuses submitted drafts and retained
attempt/retry history; use `recover` rather than bypassing those records.

### Older global drafts

Earlier versions defaulted to `~/Library/Application Support/zed-pr-review` on
macOS, or `$XDG_STATE_HOME/zed-pr-review` / `~/.local/state/zed-pr-review` elsewhere.
**Those drafts are not automatically migrated or deleted.**

Use `--draft-dir` with the **exact old per-PR directory** to `list`, `edit`, or
`recover` an old draft; the offline behavior above is unchanged. You can no
longer add comments or submit directly from external state: those actions now
require a live repository-local review session. Old drafts remain untouched;
there is no automatic migration.

Check the old draft before starting a new local one. In particular, recover an
uncertain submission in its original directory; do not copy its comments into
a fresh draft or send another review merely because the new default appears
empty. Separate state directories cannot prevent duplicate submissions across
them.

## Submission safety and recovery

Each submission (`submit-comment`, `approve`, `request-changes`, or finish choices
2–4) sends **one POST** containing the summary, all saved inline comments, and the
pinned head as `commit_id`. The events are `COMMENT`, `APPROVE`, and
`REQUEST_CHANGES`, respectively. The CLI rechecks the checkout, PR identity,
head/base, and comment coordinates before sending. A comment-only review is
allowed on your own PR; approval and request-changes reviews are not. All three
refuse submission while you already have a pending GitHub review. Resolve that
pending review on GitHub first; this tool will not modify it.

GitHub has no atomic expected-head condition for this operation. A PR can change
between the final check and the POST. The review still targets the pinned head;
check GitHub if the CLI warns that the PR changed during submission.

Before the POST, the CLI saves the exact attempt and the IDs of existing reviews.
An error, interrupted process, or missing receipt may mean GitHub accepted the
review even though `gh` failed. The saved attempt blocks edits, discard, local
cancellation, and another submission. Cancellation also refuses retained retry
history, even after a retry was authorized. **Do not retry or abandon locally
just because `gh` failed.** Use `recover`; cancellation cannot resolve uncertainty
or clean a submitted draft.

1. Run `recover`, using `--draft-dir` if the checkout or PR revision changed.
2. Recovery checks new reviews by the same reviewer for the decision, exact
   summary body, pinned head, and full set of comment bodies and original
   coordinates. It can recover after comments move, after the PR closes, or
   after a review is dismissed. A dismissed review is recorded as `DISMISSED`,
   not as an active approval. One match records the receipt and marks the draft
   as submitted.
3. Multiple matches, or new reviews with no matching attempt, keep recovery
   blocked. Inspect GitHub rather than forcing another POST. Recovery also
   refuses to proceed if the authenticated account changed. Edited review
   summaries or comment bodies can prevent an exact match.
4. No match is not proof of failure: a delayed server write is possible. Check
   GitHub yourself. Only then use `recover --retry` and type the literal `yes`
   when prompted. With no new review, this archives the old attempt and enables
   a later retry. It **does not post a review**. Run `submit-comment`, `approve`,
   or `request-changes`, or select the corresponding action in `finish`, only if
   another attempt is actually needed. Previous
   attempts remain in the draft and are checked both before opening the editor
   and before another POST. If an earlier submission becomes visible, the CLI
   records its receipt instead. No client can eliminate a delayed-write race
   when the server provides no idempotency key.

## Tests and validation limits

The tests run offline and require no Python interpreter. Integration tests use
real local jj/Git objects and the Rust `zpr-test-helper` executable for fake `gh`,
Git/jj wrappers, hooks, editor/Zed commands, delay gates, and terminal drivers.
Cargo builds the helper once. Each isolated fixture installs symlinks with
adjacent JSON configuration; the helper never falls back to real `gh` or network
transport. Ordinary Git/jj calls still execute the real local tools. Navigation tests redirect stdin and stdout and
require `stack --watch` to run `jj l` once in local mode, with no viddy or GitHub
calls. Review regression tests cover one-shot output and checkout guards. PR
terminal tests cover colors, scrolling, automatic and manual refresh, failed-read
recovery, active-review changes, concurrent review commands, and terminal
restoration on exit. Stack tests cover flat linear graphs, fork lanes, metadata
escaping, pagination, exact branch relationships, ambiguity, and changes during
navigation. Local transport tests
cover fetching, tracking, pending-ref refusals, complete parent/child navigation,
and preserved drafts. Immutable-head tests cover empty-child checkouts, review
commands, navigation, and protection of ignored files. Integration cleanup tests
cover approval and request-changes reviews, recovery, interrupted deletion,
retained comments, and navigation after snapshots are removed. Guard tests verify
that review actions outside a live diff do not read stdin, open an editor,
contact GitHub, or create state.

The finish-menu unit tests cover all four numbered choices, arrow selection,
Enter with no selection, dismissal keys, single-line non-TTY input, and rendering
with Ratatui's test backend. Session unit tests cover local cancellation,
archives and retained metadata, partial cancellation and retries, isolation from
other reviews, rejection of old cancelled tabs, and cleanup with a `COMMENTED`
receipt. These are not proof of the full Zed workflow or live GitHub behavior.
The tests do not exercise the Zed GUI or mutate live GitHub reviews.

The helper lives in `tests/support/`. `gh.rs` implements the stateful and ordered
GitHub fixtures; `transport.rs` and `cli_tools.rs` implement the command wrappers.
`pty.rs` uses Unix PTYs through `libc` and an incremental `vte` parser. The three
terminal drivers preserve mouse/navigation, refresh, terminal-restoration, editor
handoff, and signal checks. Processes are bounded and cleaned up on failures.
`helper_gh.rs` tests the fixture's own argument validation, mutations, failure modes,
and concurrent reads. The existing JSON controls and request-log formats remain
compatible. No helper code runs in the production CLI; use `--bin zed-pr-review`
when building only the application.

For a quick feedback loop, start with unit tests (run these from this directory):

```sh
cargo test --locked --lib --bins -- --test-threads=4
```

Then run the integration tests for the changed feature. Transport cases are
individually selectable; the former group names remain valid filters:

```sh
# One transport case, rather than every checkout safeguard in both layouts:
cargo test --locked --test checkout_transport local_transport_colocated_and_noncolocated::colocated::dirty -- --exact
# All ignored-file safeguards, in both repository layouts:
cargo test --locked --test checkout_transport ignored_files_colocated_and_noncolocated -- --test-threads=4
# Local comments and background preparation:
cargo test --locked --test cli cached_comments -- --test-threads=4
cargo test --locked --test cli prefetch -- --test-threads=4
```

Run the full suite before finishing a change that affects multiple features:

```sh
cargo test --locked --manifest-path ~/.config/zed/zed-pr-review/Cargo.toml -- --test-threads=4
cargo clippy --locked --manifest-path ~/.config/zed/zed-pr-review/Cargo.toml --all-targets -- -D warnings
```

The full suite still takes minutes: it creates hundreds of independent Git/jj
repositories and runs real fetches, imports, and checkouts against local fixtures.
Use four test threads as a starting point, rather than running one subprocess-heavy
test per CPU. Test fixtures limit jj's Rayon pool to one thread so test-level
parallelism does not multiply into many pools; production settings are unchanged.
One terminal test also deliberately waits for the real 30-second refresh interval.

Rust's `has been running for over 60 seconds` message is informational, not a
failure or a timeout. Previously, one transport test name covered as many as 24
sequential scenarios. Those scenarios now run as separate tests, with the same
coverage and isolation. Long-running notices now identify individual transport
cases rather than entire groups. Do not remove tests or raise timeouts just to
hide that message.

For just the navigation regressions:

```sh
cargo test --locked --manifest-path ~/.config/zed/zed-pr-review/Cargo.toml --test cli navigation_ -- --test-threads=4
```

For the finish dialog and session unit tests, run these from this directory:

```sh
cargo test --locked --lib review_menu::tests
cargo test --locked --lib session::tests
```

For the PR terminal regressions, run `cargo test --locked --test cli stack_terminal`
from this directory. These tests use a real pseudo-terminal with an offline
GitHub fixture, not the Zed GUI. Check shortcut handling, pane layout, and saving
buffers in Zed separately.

The selection contract was checked against the Zed 1.29.3 source, with the same
relevant code in 1.29.2. This is a version-specific constraint, not a guarantee
for other versions or a successful GUI test. The actual GUI workflow has not
been tested. These tests do not verify live shortcut handling, message-tab
behavior, or GitHub submission end to end.

Manual validation of the finish flow should check:

- Editor Ctrl-Shift-X opens the terminal dialog; 1–4 choose immediately, arrows
  plus Enter choose deliberately, and dismissal leaves the draft and snapshots.
- Choices 2–4 use the summary editor. A blank saved summary submits nothing and
  retains the draft and snapshots; a nonblank summary includes saved inline comments.
- Choice 1 archives locally, removes only the matching snapshots and active
  pointer, retains the return receipt, leaves the checkout unchanged, and does
  not close Zed tabs. Old cancelled tabs must not affect a newly opened review.
- Cancellation works offline after checkout or remote changes, refuses submission
  attempts, retry history, and submitted drafts, and can resume from the old exact tab
  before a new session is created.

These are validation targets, not completed GUI or live-submission checks.

Sources for the Zed integration:

- [Selection context](https://github.com/zed-industries/zed/blob/v1.29.3/crates/editor/src/runnables.rs#L482-L551)
- [Selected text availability](https://github.com/zed-industries/zed/blob/v1.29.3/crates/project/src/task_inventory.rs#L1000-L1031)
- [Task variables exported as environment variables](https://github.com/zed-industries/zed/blob/v1.29.3/crates/task/src/task_template.rs#L251-L291)
- [Project Diff uses the right-side editor](https://github.com/zed-industries/zed/blob/v1.29.3/crates/git_ui/src/project_diff.rs#L547-L566)
- [Directory diff support](https://github.com/zed-industries/zed/blob/v1.29.3/crates/cli/src/main.rs#L165-L251)
