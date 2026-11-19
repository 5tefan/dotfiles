---
name: stefan-stack-qa
description: Stefan's QA process for a stack of jj revisions in the compa repo. It builds and serves each revision on its own, seeds realistic data, and captures 1440×900 screenshots per bookmark into scratch/<name>/<bookmark>/. It writes STACK.md (how each revision fits the stack), CATALOG.md (one plain-English caption per image, used verbatim in PR test plans), and a report of findings. Use when asked for QA screenshots, a QA report, PR test-plan images or captions, or to check a stack's UI revision by revision.
---

# Stefan's stack QA

The output is a set of artefacts Stefan attaches to PRs. Each screenshot must
show exactly what its own revision adds, nothing from PRs above it. Follow
`stefan-stack-dev` for jj and editing rules. This skill covers the QA process
itself.

## Output layout

```
scratch/<name>/
  STACK.md                 # every revision in the stack, how it fits
  CATALOG.md               # every kept image + the caption that goes above it in the PR
  report.md                # how the shots were made, findings, limitations
  report.html              # same, with inline thumbnails (optional)
  seed_*.py, capture_*.mjs, serve_rev.sh   # reproducible tooling
  <bookmark>/              # e.g. dqms-agent4/, internitty73/
    <NN>-<screen>.png      # e.g. 01-admin-index-entry.png, 02-approver-list.png
```

- `<name>` is the stack or effort, e.g. `dqms-agent` or `internal-audit`; ask if
  it's unclear. `<bookmark>` drops the `stefan/` prefix.
- Screen names are short, kebab-case, and describe the state, not the PR. Prefix
  them with a two-digit number in review-flow order. Use `--` for a variant:
  `--decision` (scrolled to the decision row), `--push-dialog`, `--1024`
  (narrow viewport).

## STACK.md

One row per revision in stack order, bottom first. Include revisions with no UI,
so the table explains the whole stack.

```markdown
# <name> stack

| Bookmark        | PR     | How it fits                                                                                            | Screenshots        |
| --------------- | ------ | ------------------------------------------------------------------------------------------------------ | ------------------ |
| internal-audit1 | #37155 | Adds the tenantless `InternalAuditEvent` table that internal-agent actions are audited into.           | —                  |
| internal-audit2 | #37156 | Refactors `@mutation` around a shared builder so a second decorator can reuse it; no behaviour change. | —                  |
| dqms-agent4     | #…     | Shows "agent on behalf of X" on issues and comments the agent wrote.                                   | 2 (see CATALOG.md) |
```

- "How it fits" is one or two sentences. Say what the revision adds and why the
  revision above needs it, not how it's implemented. Write it from the
  revision's diff and description.
- Get PR numbers from the bookmark's GitHub PR if known; otherwise leave "—".
- Regenerate the table when the stack changes (inserted `N.5`, dropped or merged
  revisions), and mark merged revisions "merged".

## CATALOG.md

The single source of caption text. Each Description is copied verbatim as the
line above that image in the PR test plan, so write it for a reviewer who has
only the PR in front of them.

```markdown
# <name> screenshots

| Revision               | Image                                           | Description                                                       |
| ---------------------- | ----------------------------------------------- | ----------------------------------------------------------------- |
| cross-tenant-approval1 | cross-tenant-approval1/01-admin-index-entry.png | New internal cross tenant data access approvers shows up in admin |
| cross-tenant-approval1 | cross-tenant-approval1/02-approver-list.png     | Can see list of approvers                                         |
| cross-tenant-approval1 | cross-tenant-approval1/03-add-approver.png      | Can add a new approver                                            |
```

- One row per kept image. Rows go in stack order, and within a revision in
  review-flow order: starting state, then actions, then outcomes.
- Image paths are relative to `scratch/<name>/`.
- Write the description while inspecting (step 5): open the image next to the
  revision's diff and say what feature the image shows.
- Update the catalog whenever images are added, deleted or recaptured.

### Writing descriptions

- One line, plain English, about 5–15 words. Say what the reviewer is looking
  at in product terms: "Can add a new approver", "Rejected review shows the
  reviewer's reason".
- Describe the feature, not the process. Never mention QA setup or provenance:
  no "stack-tip QA", "throwaway database", "fake users", "seeded", "not rerun",
  "revision QA", "does not verify migration", commit hashes, tenant slugs, or
  report section numbers. Those go in report.md, if anywhere.
- No hedging or test-coverage claims. A screenshot shows a screen; it doesn't
  "verify" anything.
- If the image shows a bug, say so plainly: "Pending AI review incorrectly
  shows a low-risk badge". Never caption a broken screen as working.
- For before/after pairs, start with "Before:" / "After:".
- Bad: "Supplied stack-tip QA in throwaway qa_xtenant with fake users shows the
  Users admin entry; not rerun here and does not verify migration execution."
  Good: "New internal cross tenant data access approvers shows up in admin".

## Process

### 1. Scope

- List the stack's revisions:
  `jj log --ignore-working-copy -r '<first>::<last>' -T 'bookmarks ++ " " ++ description.first_line() ++ "\n"'`.
- For each, read its diff (`jj diff --ignore-working-copy --stat -r <bm>`) to
  decide whether it changes UI, and which screens and states show the change.
- Write the plan down before capturing: per bookmark, the screens and states.

### 2. Seed realistic data

- Write a seed script under `scratch/<name>/` and run it through
  `manage.py shell` (intern settings for intern data). It must be idempotent:
  delete its own previous rows first, matched by a `[QA]` title prefix or a
  fixed id.
- Use the QA user `diff-review-ux@compa.ai` on tenant `lilac_greenhouse`. Create
  it if missing, and don't reset its password on every run, or the browser logs
  out.
- Make data realistic: plausible titles and bodies, real relations, several
  authors, system rows, timestamps spread over hours.
- `PrivacyAwareQuerySet` forbids `.update()`/`.delete()`, so save or delete
  objects one by one, under an all-powerful context.
- When a scenario needs the user's own state (a running sandbox, a summon), make
  it explicit in the script's arguments (`SCENARIO=…`, `SETUP=1`).

### 3. Serve one revision at a time

Never capture a lower PR from the tip. Use a throwaway server per revision
(`scratch/<name>/serve_rev.sh <bookmark>`):

- Create a jj workspace in the scratchpad at the bookmark. Guard every `cd`.
- Link `.venv` and both `node_modules`. If main's lockfile changed, give the
  workspace its own `.venv` with `uv sync --frozen`.
- Copy the legacy `intern/frontend/dist` and `tiger_lily/frontend/dist`; login
  still uses them. Copy, don't symlink: symlinks fail the sketchy-file system
  check and get snapshotted into a commit.
- Build intern with `NODE_ENV=production node_modules/.bin/webpack --config ./webpack.prod.js`.
- Run `DJANGO_SETTINGS_MODULE=config.internal.settings manage.py runserver 127.0.0.1:8011 --noreload`
  under Doppler, and wait for `/_health` to return 200.
- `--stop` kills the server, forgets the workspace and deletes it. Always stop
  it at the end.
- Memory is tight: a build takes 35 minutes with the dev stack up and about 35
  seconds without it. Stop `systemctl --user stop compa-honcho.service` first,
  tell Stefan, and start it again when done.
- Loop over the bookmarks in a background script (`run_all.sh`): serve, capture,
  stop, with one log line per revision. Read the log when it finishes.

### 4. Capture

- Use Playwright from `e2e/` (copy the script in as `_x.mjs`, run it, delete
  it), with `PLAYWRIGHT_HOST_PLATFORM_OVERRIDE=ubuntu24.04-arm64`.
- Login is two steps: fill the email, submit, fill the password, click "Log In",
  then wait until the URL isn't `/login`.
- Viewport 1440×900. Never full-page. Scroll the relevant element into view
  (`scrollIntoView({block: "start"})` for sections, `center` for buttons and
  decision rows) and wait about 500 ms before each shot.
- Capture every state the PR adds: empty, loading, success, error, disabled,
  dialogs (open, filled, acknowledged), blocked or stacked variants, and the
  list view if it changes.
- Capture the same set of screens at every revision of a feature series, so
  adjacent PRs compare like for like.
- Record GraphQL failures on `graphql-intern` responses (status ≥ 400) and print
  them. Any error means the shot is wrong; find out why first. A
  "Request falls outside acceptable limits" means the query exceeded the field
  limit.

### 5. Inspect every image and write the catalog

- Open every PNG alongside the revision's diff. Check that it shows what that
  revision adds and nothing from later revisions (e.g. agent4 must not have
  agent5's "System" label).
- Delete duplicates and useless shots (a scroll that changed nothing, a list
  with the column off-screen).
- Add a CATALOG.md row for each kept image, following "Writing descriptions".
- Note real bugs the screenshots reveal: missing punctuation, wrapping buttons,
  overflowing inputs, misplaced indicators, broken pages. Point to the file and
  line. Don't fix them unless asked.
- If something can't be captured (an expired sandbox, data by design on another
  page), say why in report.md and show the closest true state.

### 6. Report

`report.md` is for Stefan, not the PR. It opens with one paragraph on how the
shots were made (revisions served, seed data, anything reused or not rerun),
then a section per bookmark with findings only:

```markdown
## dqms-agent10: issue links and "Mark as related"

Images: 3 (see CATALOG.md)

Found:

- "Mark as related" wraps onto two lines next to the full-width input
  (`intern/frontend/src/.../RelateForm.tsx:42`).
```

- Provenance, staleness and limitations belong here, never in CATALOG.md.
- Optionally render `report.html` with thumbnails and the catalog descriptions
  (Python-Markdown via `uv run --no-project --with markdown`), and check it
  renders.

### 7. PR test plan

When Stefan asks for a test plan, or pastes his PR's uploaded images, return
paste-ready markdown for the PR's test plan section, and nothing else in it:

```
- includes tests 🧪

New internal cross tenant data access approvers shows up in admin
<img width="1440" height="900" alt="01-admin-index-entry" src="https://github.com/user-attachments/assets/..." />

Can see list of approvers
<img width="1440" height="900" alt="02-approver-list" src="https://github.com/user-attachments/assets/..." />
```

- `- includes tests 🧪` only when the PR's diff adds or modifies test files
  (check `jj diff --stat`); factories, fixtures and Storybook stories don't
  count. Never describe the tests or list test commands.
- Then, per image in CATALOG.md order, its Description verbatim on one line,
  then the image. At most one line above each image.
- Map pasted URLs to catalog rows by alt text or filename. Keep his URLs and alt
  text exactly. If a pasted image has no catalog row, or doesn't match its
  row's description, look at it, fix CATALOG.md, then use the fixed line.
- No headings, notes, provenance or summaries in the section.

### 8. Clean up

- Stop the revision server, forget its workspace, and abandon any stray commits
  it left (empty commits, or snapshotted `dist`/`node_modules` symlinks).
- Remove temporary `e2e/_*.mjs` files.
- Restart `compa-honcho.service` and confirm 8000 and 8001 answer.
- Leave the seed, capture and serve scripts in `scratch/<name>/` so the run can
  be repeated.
- Tell Stefan what was captured, where (CATALOG.md), what's broken, and any
  local database changes his Mac needs (e.g. re-applying an edited migration).
