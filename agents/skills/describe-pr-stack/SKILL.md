---
name: describe-pr-stack
description: Update descriptions across a stack of GitHub pull requests with one parallel subagent per PR. Each agent reads, drafts, updates via gh, and verifies its PR. Use for filling or revising descriptions, attaching QA screenshots, moving branch-stack navigation, or adding requested reviews. Preserve author text and generated metadata, then reconcile the whole stack after automation settles.
---

# Describe a PR stack

Treat this as a metadata-editing task, not a code change. **For multiple PRs, fan out one parallel subagent per PR. Each agent reads the existing PR, inspects its diff, writes the description, updates it with `gh`, and reads it back.** The coordinator prepares shared context, assigns every PR, monitors automation, and verifies the whole stack afterward.

Use direct `gh` commands and built-in file tools. Do not write custom scripts, text transformers, or bulk mutation loops for this task. A large stack is a reason to use parallel agents, not to replace them with a script or route every write through the coordinator. A successful API response is not proof that stack automation left the edit intact.

## 1. Coordinator: establish scope and shared context

- Resolve the repository with `gh repo view --json nameWithOwner`. Discover open PRs with a sufficient limit or pagination, then match exact head branches. Do not infer scope from a search prefix, title numbering, or consecutive PR numbers.
- Respect an explicit branch range. For an entire stack, enumerate its base/head relationships and state the resulting scope; ask only if the boundary is genuinely ambiguous. Do not add a neighboring stack or newly discovered successor to a bounded request.
- Keep discovery output compact. Collect identities first rather than dumping unrelated PR bodies. For each selected PR, record its number, exact head branch, base branch, head SHA, title, and draft status. Collect review requests only when needed.
- Read the supplied overview, QA report, repository PR template/rules, and author examples once for shared context. Map each overview entry and QA section to its exact PR. Examples guide style; agents still inspect each PR's own evidence.
- In the compa repository, consult `.github/pull_request_template.md`, `.claude/rules/pull-requests.md`, and `.cursor/skills/pr-title-and-description/SKILL.md`. In other repositories, discover and read their PR templates and rules instead. Explicit user requirements take precedence over style defaults. Otherwise, apply the large-stack navigation rule below even when the template puts navigation at the top.
- Record the requested common text, field values, test-marker spelling, section removals, and screenshot requirements in shared instructions. Do not turn values from an earlier task into defaults for a new stack.
- Carry the minimal Test Plan rules in section 4 into shared instructions: only a qualifying test marker followed by relevant screenshots with brief plain-text orientations. These are firm limits, not style defaults. Preserve substantive author-written verification and material evidence caveats. Treat the QA report as evidence to inspect, not prose to copy into PRs. Do not ask agents to add execution disclaimers or inspection details to PR bodies.
- Inspect workflows that write PR bodies, including callers of reusable workflows. Record their triggers and whether they can write neighboring PRs. If attachments are needed, check `gh pr edit --help` for the installed CLI's capabilities before assigning uploads.
- Create task-local scratch space with built-in file tools and give each agent unique paths. If an ignored QA directory appears missing or empty in a file tool, confirm with read-only `ls -la` and exact-path reads before declaring the evidence unavailable.

Do not change titles, bases, bookmarks/branches, commits, code, draft status, or reviewers unless explicitly requested. Use explicit repository and PR identities on every PR command. Do not check out branches to write descriptions. For local inspection in a jj repository, use `JJ_PAGER=cat jj --ignore-working-copy`.

## 2. Coordinator: fan out one agent per PR

Launch independent assignments together using the available parallel tool mechanism. Keep one owner per PR, even for a stack with dozens of small PRs. If tool concurrency limits require waves, retain individual assignments and launch each wave concurrently. Do not wait for each PR to finish before starting the next, assign ranges merely to reduce the agent count, or limit agents to drafting while the coordinator performs all writes.

The coordinator handles shared preparation and final reconciliation, not duplicate per-PR inspection. Agents may read the same overview and QA files, but may write only their assigned PR and unique scratch files. The coordinator must not edit a PR while its owner is active. If subagents are unavailable, explain that limitation and follow the same per-PR cycle directly; do not silently substitute a bulk script.

### Assignment contract

Provide each agent with all of the following; an agent does not inherit the coordinator's conversation:

- Repository, PR number, exact head branch, expected base branch and head SHA.
- This skill, shared instructions, repository rules, and the relevant overview entry and QA section, with full paths or exact excerpts.
- The user's exact common text, field values, section changes, test-marker wording, and screenshot requirements. Include the navigation placement decision and known automation risks.
- The minimal Test Plan contract from section 4: a marker describes actual test changes, not execution; screenshots have brief plain-text orientations above them; agent-process filler is prohibited. These are firm limits, not optional guidance. Leave a required heading empty when neither tests nor screenshots qualify, while preserving substantive author verification unless replacement was requested. Keep detailed findings in recovery files or the report to the user, and retain genuine failures or material limitations briefly beside the relevant evidence. Do not add contradictory requirements from shared QA notes.
- The original body if already collected, but require a live read before drafting and another immediately before writing.
- Unique paths for the recovery files below.
- Explicit authority to update this PR with `gh` and attach the requested evidence, but no other PR or repository changes.
- The required result fields and the instruction to report uncertainties rather than claim success.

Each assignment is: **read → inspect → draft → fresh-read → update → read back → report**. Resume the same agent for unresolved work rather than creating a second writer for its PR. A response such as “updated” without evidence and recovery paths is incomplete, not success.

## 3. Agent: complete the assigned PR

1. Read the live PR body and metadata with `gh pr view`. Save the original body. Confirm the repository, PR number, and exact branch match the assignment.
2. Inspect `gh pr diff` against this PR's own base, not cumulatively against the stack's root. Check source changes, actual test changes, and migrations; filenames and titles alone are insufficient. Read generated lockfiles structurally when appropriate and record significant inspection gaps in recovery files or the report to the user. Include a gap beside PR evidence only when it materially limits that evidence. For local inspection, a revision diff is sufficient only for a one-revision PR; otherwise compare the actual base and head.
3. Draft the body using the content, screenshot, and metadata rules below. Save the proposed body with built-in file tools. Review the intended changes before publication; do not require a separate coordinator approval unless the user requested one.
4. Immediately before writing, re-read the body and head/base. Start from that fresh body, retaining its current generated navigation and dependencies. Compare original, intended, and live text; apply nonconflicting edits and preserve intervening human changes. Report conflicting human edits rather than restoring the earlier proposal over them. If the head or base changed, inspect the current diff and reassess the description, test marker, and QA evidence before proceeding. Report drift; never restore an old branch or base to fit the assignment.
5. Update only this PR with `gh pr edit`, an explicit repository/PR, and `--body-file`. Use supported attachment flags when needed. Use bounded command timeouts, not shell-interpolated Markdown or terminal commands that create/edit body files. Skip a write when the live body already meets the requirements.
6. Read the live body back even if the command failed. Treat timeouts, HTTP errors, and partial uploads as unknown outcomes, not rolled-back transactions. Record successful asset URLs from the response or read-back before any retry. Verify content and metadata, not just the command's exit status.
7. Save the intended body with uploaded URLs separately from the latest observed body. Report any mismatch to the coordinator. Do not repeatedly overwrite a PR while neighboring workflows are still writing it; the coordinator handles reconciliation after the initial agents finish.

### Recovery files

Use distinct files per PR, with unambiguous meanings:

| File suffix    | Contents                                                                                                                                                            |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-original.md` | The first live body read by the agent; never overwrite it.                                                                                                          |
| `-proposed.md` | The proposed body before publication; local image references may be present.                                                                                        |
| `-intended.md` | The desired body with successful uploads replaced by remote URLs. Flag any unresolved attachments in the result. Never replace this file with a reverted live body. |
| `-observed.md` | The latest body actually read from GitHub, even if automation reverted it.                                                                                          |
| `-result.json` | Structured evidence, outcomes, and recovery paths.                                                                                                                  |

Use built-in file tools for these artifacts. Keep them outside application code and do not commit them. Put detailed inspection results, QA notes, and uncertainties in `-result.json` or the report to the user, not in the PR's Test Plan. The body-shaped recovery files should contain only their respective PR bodies. For reconciliation, merge intended edits into the fresh live body; do not blindly restore a saved whole body.

### Required result

Save and return a structured result containing:

- PR number, branch, inspected head SHA and base; any subsequent drift.
- Whether the description was filled, revised, or preserved, and the resulting summary.
- Whether tests were added/modified, with supporting files or diff details; migrations and other content that must be preserved.
- Corrections to the supplied overview, stale QA, inspection gaps, and uncertainties.
- Screenshot count, captions, successful URLs, and failed or unresolved uploads.
- Whether generated metadata and requested section ordering were preserved.
- Write outcome and read-back result: verified immediately, unchanged, mismatched, or blocked. Immediate verification is not final whole-stack verification.
- Paths to every recovery file.

## 4. Agent: write concise, evidence-based content

Default to one or two summary sentences, three maximum, unless the user requests otherwise. Explain what changed and why. Call out pure moves/exports and security boundaries when relevant; avoid exhaustive implementation lists or promises about later PRs.

An empty body, a template-only Summary, or metadata without prose counts as a missing description. Ignore guidance comments when detecting placeholders. Preserve substantive author-written content unless the user requests its replacement. Apply requested field or section changes without unnecessarily rewriting unrelated prose.

When the user supplies descriptions in an overview, use the matching entry's wording. Correct demonstrable factual errors against the published diff and report those corrections. Preserve an explicitly supplied common paragraph verbatim and put the PR-specific description in a separate paragraph when requested.

Use the repository's sections and the author's conventions. Keep impact/task fields short and use only values supported by the request or supplied context. Preserve real checklist items, especially migration checklists, by default. An explicit request to delete the Checklist/Checklists section overrides that default, including for PRs with migrations. Remove unused template scaffolding where appropriate without deleting automation metadata.

### Test Plan

Keep Test Plan minimal: include only `- includes tests 🧪` when this PR adds or modifies actual tests, followed by any relevant screenshots. Use different marker wording when explicitly requested. Fakes, fixtures, factories, test dependencies, and ordinary Storybook examples alone do not qualify. Inspect test contents rather than treating a filename as proof. Do not add bullets or paragraphs describing test coverage or implementation details.

**The test marker describes the diff, not a test run.** Never imply tests ran during this metadata-only task merely because they exist. Avoiding a false execution claim does not require a negative disclaimer. Agent-process filler is prohibited in PR bodies: no “Tests were not run for this description-only update,” “CI runs the tests,” other execution disclaimers, coverage summaries, environment details, QA-report narratives, or descriptions of the agent's inspection and verification process.

Place a brief plain-text orientation above each screenshot, explaining what it shows in relation to this PR. Captions are not extra bullets. Keep detailed inspection results and uncertainties in recovery files or the report to the user. Preserve genuine failures or material evidence limitations briefly beside the relevant evidence; do not erase or hide them to make a Test Plan sound successful. Do not invent commands, successful output, screenshots, or coverage claims.

When neither tests nor screenshots qualify, leave a required Test Plan heading empty rather than inventing filler. Preserve substantive author-written verification unless replacement is requested; these rules do not authorize deleting it.

## 5. Agent: attach the appropriate QA screenshots

- Read the matching QA section and inspect the images before attaching them. Map screenshots to the feature introduced by this PR, not every UI PR in the stack. Put images in Test Plan after any qualifying test marker. Above each image, add a brief, accurate plain-text orientation explaining what it shows in relation to this PR, not a bullet or a narrative copied from the QA report.
- Check whether the QA revision matches the current diff. Label evidence from an earlier revision or the stack tip honestly; do not claim that it proves this individual revision passes. Preserve genuine failures and material limitations briefly beside the relevant evidence. Keep environment details, inspection steps, and other QA notes in recovery files or the report to the user, not in PR bodies. Include only a brief caveat beside the evidence when such a detail materially limits it. Report missing evidence to the user rather than fabricating it or adding a missing-screenshot disclaimer to the Test Plan.
- Use direct GitHub attachments when the installed `gh pr edit --help` supports `--attach`. With a CLI that supports rewriting local image references in `--body-file`, use that mechanism and repeat `--attach` for the needed files. Follow the installed help for limits and path resolution; do not assume every `gh` version supports this.
- For example, the CLI used in the successful workflow allowed up to 50 files per command and resolved relative references from the body-file directory, then the working directory. Treat these as capabilities to confirm, not universal guarantees. Prefer confirmed absolute paths when that avoids ambiguity.
- A request to upload QA screenshots authorizes those uploads to the appropriate PRs; do not ask for redundant permission. Do not substitute a public image host, gist, release, code commit, or browser-cookie extraction. If supported attachments are unavailable, report the limitation and ask for an approved alternative.
- An attachment failure may still leave some uploads and the body update applied, even with a nonzero exit. Read back before retrying. Save successful URLs, reuse them in later bodies, and retry only unresolved attachments. Never invent asset URLs or re-upload everything merely because automation reverted the prose.

## 6. Agent: preserve generated metadata

Treat these as separate blocks, not summary text:

- Generated branch-stack navigation and its marker, such as `<!-- branch-stack -->`.
- The explanatory comment attached to that marker.
- Dependency metadata, such as paired `stacked-pr-dependency` markers and the `Depends-On` line.

When the full stack contains more than five PRs, move its navigation to the bottom of each in-scope body unless the user explicitly requests another placement. Count the full stack, including the current PR but excluding non-PR roots such as `main`, not just the selected PRs. This applies even when only one PR is in scope; do not edit out-of-scope PRs. For stacks of five or fewer PRs, preserve placement unless asked to move it.

Move the entire existing navigation block and its attached comment below prose, checklists, and dependency metadata. Keep links, indentation, current-PR highlighting, and marker spelling intact. Move a marker-only placeholder as a block; do not invent a chart. Do not remove markers to stop automation.

Identify exact block boundaries before editing the saved body with built-in file tools. Do not use a greedy replacement that can swallow neighboring sections. Ambiguous boundaries, duplicate markers, or detached comments require inspection, not a custom text transformer. Reapplying the intended edit should make no further changes.

## 7. Coordinator: reconcile and verify the whole stack

Parallel ownership prevents duplicate agent writes, **not races from stack automation**. A workflow triggered by one PR may rewrite its neighbors from stale bodies. Even if every agent's immediate read-back passed, later automation may restore templates or older prose. Completion of one PR's workflow is not proof that the stack has settled.

1. Account for every assigned PR and collect all structured results. Follow up with the same agent on missing fields or incomplete work. Check identities and SHAs explicitly; parallel results may arrive out of order. Never identify a result by its position in the output.
2. Monitor all relevant body-writing runs across the selected stack and any neighboring PRs that can write it. Use bounded waits. Do not disable or modify workflows, cancel runs, or remove stack markers without separate authorization.
3. Once the relevant writers settle, read every selected PR's body, head SHA, and base again, including those whose descriptions were preserved. Compare identities with the inspected revisions. If a head or base changed, resume that PR's diff inspection and reassess its description, test marker, and QA evidence before declaring completion. Do not substitute agent success messages or a spot check for this full read-back.
4. Compare original, intended, and live bodies. Apply only missing, nonconflicting changes to the fresh body, preserving its current generated metadata and reusing saved asset URLs. Report conflicting human edits instead of treating them as automation reversions. Repair through the same agents or directly after their ownership ends. If races were observed, reduce concurrent repair writes and wait for the resulting workflows before checking again. Do not turn this recovery measure into a default serial first pass.
5. Bound repair attempts. If automation or new human edits keep changing bodies, retain recovery files and report the exact unresolved PRs and interference. Distinguish observed overwrites from suspected causes; do not claim branch changes based on misassociated output.

Verify all of the following:

- Every requested PR is accounted for exactly once; no out-of-scope PR was edited by an agent.
- Summaries match the inspected diffs, existing prose is retained unless replacement was requested, and supplied common text and field values meet the request.
- Test Plans follow section 4: new content is limited to a marker for actual test additions or modifications, followed by relevant screenshots and brief material evidence caveats. Preserve substantive author-written verification as required above. Marker wording matches the request or defaults to `- includes tests 🧪`; it describes the diff, not execution. Required headings remain empty when nothing qualifies and there is no substantive verification to preserve.
- No agent-process filler was introduced by agents or during coordinator reconciliation: no execution disclaimers (including “tests were not run”), “CI runs the tests,” coverage summaries, implementation details in Test Plan, environment details, QA-report narratives, or accounts of inspection and verification. Reject such additions; do not treat this prohibition as a style preference. Keep detailed results and uncertainties in recovery files or the report to the user; do not turn them into PR prose.
- Screenshots have remote URLs, belong to the correct PR, and each has a brief plain-text orientation above it, not an extra bullet. Genuine failures and material evidence limitations remain briefly beside the relevant evidence; these caveats are not filler. Report unresolved uploads to the user.
- Requested sections are removed. Checklists otherwise remain as required; stack markers and dependency targets are intact, with navigation last when required.
- Titles, draft status, branches, and bases were not changed by this task. Report changes by another actor rather than reverting them.

Check semantic content and block ordering, not merely the presence of a marker. Keep literal-source verification separate from rendered equivalence. Automation may normalize emphasis, autolinks, or `:test_tube:` to `:test\_tube:`. If verbatim text was requested, report a source mismatch even when the rendering is equivalent. Do not silently relax the requirement or endlessly fight a formatter; ask whether equivalent Markdown is acceptable when necessary.

Report the exact scope; counts of filled, revised, and preserved descriptions; test markers and screenshots; overview corrections; and final verification status. Name exclusions, failures, and any remaining automation risk. No application test run is needed solely for PR-body edits; this is workflow guidance, not a disclaimer to add to a PR. Remove only task-created scratch files after successful verification; retain useful recovery files while anything remains unresolved and give their paths.

## Optional: request team reviews

Only do this when requested. Re-evaluate the exact scope rather than silently inheriting or expanding an earlier range. Discover existing requests, resolve the exact team slug, and add only missing requests with `gh pr edit --add-reviewer`. Keep one writer per PR and preserve other reviewers and draft status. Pace requests to avoid duplicate notifications and API limits, then re-read every PR to verify the team is requested.
