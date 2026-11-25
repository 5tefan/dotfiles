---
name: stefan-stack-dev
description: >-
  Stefan's way of developing in the compa repo with stacked jj revisions and
  bookmarked PRs. Use whenever working on a stack in the compa repo (internitty,
  dqms-agent, internal-audit or any `stefan/<name>N` series): editing or inserting
  revisions, rebasing on main, fixing CI on a PR in the stack, writing tests,
  QA screenshots, or PR descriptions.
---

# Stefan's stack development

Work happens in long stacks of small jj revisions, each one a PR with a
`stefan/<stack>N` bookmark. Reviewers read each PR on its own, so every revision
must make sense, pass CI and introduce things in their final form.

## Hard rules

- **Never renumber bookmarks.** Insert between existing ones as `N.5`, `N.6`, ….
  A new revision on top takes the next number. Say which bookmarks you created.
- **Report every deleted bookmark**, including ones jj deletes for you when a
  revision is abandoned, and whether the branch still exists on GitHub.
- **No merge migrations.** On rebase, compare main's new migrations with the
  stack's per app. On a clash, renumber or re-point the stack's migration. A
  migration that only exists in the stack may be edited in place.
- **No new code comments.** Don't add comments, and take out any you added
  earlier in the session. Leave existing ones alone. Generated headers are fine.
  The `.py` comment-ratio CI check (4% of added lines) enforces this.
- **Commit and push only when asked.** Never push, delete remote branches or
  run `jj git push --deleted` on your own.
- **"Don't make changes" means discuss only.** Orient, answer and propose.
  Wait for an explicit go.
- **Secrets:** never write to prod Secrets Manager from local dev. Sandboxes
  never hold GitHub, AWS or OpenAI credentials. Never read or change Doppler
  (and never prod Doppler) without asking.
- **Intern internal agents:** no `active_tenant(...)` in agent tools. No tenant
  audit log for internal agents. They use `InternalAuditEvent` through
  `@internal_mutation`.
- **Don't edit stacks or branches outside the task**, such as the sandbox-infra
  stack or a branch already enqueued for merge.

## Shape of a stack

- Order revisions by dependency: data model, core rules and validation,
  services, background work, API, UI, then integrations and extras, polish
  last. Each PR can then be reviewed and tested without the ones above it.
- Add the safety boundary before the capability it guards, so no revision
  ships an unguarded version.
- Background state machines need a way out of stuck states: a sweep that fails
  or recovers anything left running or pending when a worker dies.
- Re-running a process resets everything it produced (result, error,
  timestamps), not only its status.
- The UI shows a result only in the state that produced it, e.g. a verdict
  badge only on a completed review.
- Keep `scratch/stack_overview.md` up to date: one row per revision with what
  it adds and why the next one needs it. Update it whenever a revision's
  behaviour changes.

## Shape of a revision

- One thing per revision. Split refactors from behaviour changes, and test-only
  prep from the code change it enables. Insert an `N.5` before a refactor if
  tests must change first.
- Introduce things in their final state. Don't add a UI or model and redesign it
  a few revisions later; fold the final design into the introducing revision.
- When a later fix belongs to an earlier revision, put it in that revision
  (`jj squash --into`). Don't add a fix-up on top.
- Prefer explicit structure over clever indirection. No enum flags that pick
  behaviour when each caller can pass the function it means. No lambdas that
  exist only so tests can patch something.
- Delete what's unused, such as the cloudflared tunnel script and middleware
  after cloudflared went away.
- Models: no `on_delete=PROTECT`; use `SET_NULL` with `null=True`, or `CASCADE`
  where it fits. Add a `CheckConstraint` for enum fields. Explicit `db_table`.
- Code style: one kwarg per line in calls with several arguments. No nested
  comprehensions. Arrow functions in TS/JS.

## Tests

- No mocks of internals. Never assert which function was called or with what
  arguments. Run the real code and check the recorded effect: database rows,
  responses, state. Write a small helper when the check repeats, e.g.
  `recorded_audit_events(tenant, *fields)`, which returns tuples of the chosen
  fields, oldest first.
- Mock only real external boundaries (third-party APIs, LLMs, GitHub, E2B), and
  use the repo's fakes (`FakeGithubClient`, `FakeSandboxGateway`) and
  `use_*_FOR_TESTING_ONLY` hooks.
- Tests must not depend on a developer's Doppler config. CI has no GitHub app,
  for example. Pin such settings with `override_settings` (e.g.
  `INTERNAL_AGENTS_GITHUB_CLIENT_ID=""`), and reproduce CI by running with
  those env vars unset.
- pylint ignores unused parameters named `_*` and `info`, so call a test
  mutation's unused input `_input`.
- Run the Django runner, never pytest:
  `doppler run --project compa --config dev_stefan -- env -u GDAL_LIBRARY_PATH -u GEOS_LIBRARY_PATH [DJANGO_SETTINGS_MODULE=config.internal.settings] .venv/bin/python manage.py test <labels> --noinput --parallel 4`.
  Use intern settings for `intern.*` labels. Use `--noinput` without `--keepdb`
  when a migration in the stack was edited in place.
- Run mypy as CI does: `mypy tiger_lily/ intern/`. A single-file run can crash
  inside mypy on `tiger_lily.viewer_context`; that isn't a real failure.
- Check disk space before heavy runs. When a full run is OOM-killed or can't
  run (e.g. full mypy with the dev server up), run a scoped version and say
  which one ran.

## jj workflow

- Don't edit in the user's working copy; the user edits it concurrently. Make a
  temp workspace in the scratchpad, `jj new <rev>` inside it, edit, then
  `jj squash --into <rev>` and `jj workspace forget`.
  - Guard every step: `cd "$W" || exit 1; [ "$(pwd)" = "$W" ] || exit 1`. An
    unguarded failed `cd` runs the next command in the user's working copy.
  - Give the workspace its own `.venv` (`uv sync --frozen`) when main's lockfile
    differs from the shared env. Install `node_modules` in the workspace for
    codegen when main bumped frontend deps. Symlinked `dist`/`node_modules` get
    snapshotted as files or trip the sketchy-file check, so don't symlink them.
- Read with `--ignore-working-copy`. The working copy is often stale, and
  `update-stale` can fail with EPERM on the shared folder. Retry it, then
  abandon the stray `lsplpvov`-style copies the failures leave.
- After any stack edit, snapshot every revision's diff first (md5 of the `+`/`-`
  lines per bookmark), then compare afterwards. Only the revisions you meant to
  change may differ. Also check `conflicts()` and `divergent()` across the
  stack.
- Then run the affected tests and the typecheck at each revision you changed,
  not only at the tip.
- `.jj/repo` is shared between the Mac and the VMs over virtiofs, and jj's lock
  doesn't hold across machines. Overlapping fetches cause "reconcile divergent
  operations", which can rewrite main into conflicted mutable copies.
  - Fix: `jj bookmark set main -r main@origin --allow-backwards`, then abandon
    the copies from that reconcile window after confirming nothing named points
    at them.
  - Then `jj git export`.
- `agent/cloud-agent-e2b-host-checks-f1f1@origin` returns with every fetch and
  makes the stack divergent.
  - Fix: `git update-ref -d refs/remotes/origin/agent/...`, then
    `jj bookmark forget --include-remotes <name>`, then abandon the old chain
    after checking it holds nothing named.
- Record the operation id before risky cleanup, so `jj op restore` can undo it.
  Record it before every stack edit or rebase too, and end the report with
  `jj op restore <id>`.
- When the same change lands in several revisions, script it: one transform
  whose replacements must each match exactly once, run on each revision. Don't
  hand-edit each copy. Regenerate generated files with a loop over the
  revisions the same way.

## Rebasing on main

1. Check how far main moved, which merged PRs are the stack's own, and main's
   new migrations versus the stack's.
2. Rebase from the first unmerged revision onto `main@origin`.
3. Resolve conflicts in the right revision.
   - Keep main's behaviour where main improved something the stack also touched.
   - Regenerate generated files (`gql.ts`, `graphql.ts`, `schema.graphql`)
     rather than hand-merging. Run intern-only codegen (schema dump,
     `yarn format-graphql:internal`, `yarn codegen:internal`, then the `Maybe`
     rewrites) at each conflicted revision, bottom-up.
4. Abandon local copies of revisions superseded by the merged versions, and
   report which bookmarks went away.
5. Verify per-revision diffs, conflicts and `makemigrations --check`, then run
   tests at the tip.

## QA screenshots for PRs

Follow the `stefan-stack-qa` skill. It serves each revision on its own, captures
screenshots into `scratch/<name>/<bookmark>/`, and writes `STACK.md`,
`CATALOG.md` (the caption above each image in the PR) and a report.

## PR descriptions

Use `.github/pull_request_template.md`. The title is `[Emoji][Project] Title`.
The Summary is one or two sentences. Impact is one line. The test plan holds
exact commands and their output, or screenshots. Put drafts in `scratch/`.

## Working with Stefan

- Lead with the answer and keep it short; detail on request.
- When something breaks or a claim turns out wrong, say so plainly with the
  evidence.
- When a teammate's review suggests a design change, apply its spirit (e.g. no
  shared dispatcher) and show the before/after briefly.
