# Shared agent skills

Each folder in `skills/` contains one skill, including its `SKILL.md` and any
supporting scripts, references, or assets. This repository holds the shared copy.

Dotbot creates an individual symlink for each skill in both locations:

- `~/.agents/skills/`: Zed's native agent.
- `~/.claude/skills/`: Claude Code, including Claude running through ACP in Zed.

Other agents may use a different discovery directory or support different skill
features. Check their documentation before adding another link destination.

## Install and update

Run the usual `./install` from the repository root to install your dotfiles,
including the shared skills. The installer also updates submodules and configures
shell startup files. Skill links use `force: false`, so an existing real file or
folder is not deleted to make room for a link. Back up and deliberately migrate
any existing skill with the same name before installing.

Edit existing skills here. Both agents read the same files through their links.
To add a skill, create `skills/<name>/SKILL.md`, include any supporting files in
that folder, then run the installer to create its links. Only skill folders
belong directly under `skills/`; do not nest them in category folders.

If you rename or remove a skill, remove its old symlinks from both discovery
directories. The existing installer does not clean those directories. Make sure
a path is a symlink into this repository before removing it.

Skills created through an agent's UI are not automatically added to this repo.
Move ones you want to share here and replace their original folders with links.
Leave agent-specific skills and Claude's managed `~/.claude/skills/synced/`
directory outside the repo. Do not link the whole `~/.claude/` directory.

## Included skills

- `stefan-stack-dev`: development rules for Stefan's stacks in the Compa repo.
- `stefan-stack-qa`: Compa's per-revision QA and screenshot workflow. It relies
  on the Compa application, its QA data, and the documented development tools
  and services; global installation does not make that environment portable.
- `describe-pr-stack`: descriptions and QA evidence for a stack of GitHub PRs.
  It requires an authenticated `gh` CLI. Parallel subagents are preferred, with
  a direct per-PR fallback when the agent does not support them.

The first two skills were migrated from `~/.claude/skills/`.
`describe-pr-stack` was moved out of
`~/code/compa/.agents/skills/describe-pr-stack/` so a project-local copy cannot
shadow the shared skill. Other Compa checkouts may still contain a local copy;
review and remove or reconcile it before expecting the shared version to load.

Original copies from the migration are retained in `.backups/2026-11-19/`,
which is ignored by version control. These backups are local to this machine.

## Compatibility and safety

Use explicit `name` and `description` fields in YAML frontmatter. Names should
match their folder and use lowercase letters, numbers, and single hyphens.
Keep descriptions concise and specific about when the skill applies.

Keep shared instructions independent of agent-specific tool names. Reference
bundled files relative to the skill folder. Do not rely on Claude-only features
such as argument substitution, shell-command injection, hooks, forked contexts,
or tool permissions in a skill intended for Zed. Both support
`disable-model-invocation: true` for manual-only workflows; this does not replace
each agent's permission settings or the user's authorization for side effects.

Do not store credentials, generated reports, or downloaded skill caches here.
Review private work material and third-party licenses before sharing the repo.
Standing instructions, such as `config/zed/AGENTS.md`, remain separate from
these task-specific skills.

## Verify installation

Check that all three skills appear in Zed's **AI > Skills** settings and in
Claude Code's `/skills` list. Invoking a skill to inspect its instructions does
not authorize pushes, deployments, PR edits, or other side effects.

See [Zed's skills documentation](https://zed.dev/docs/ai/skills) and
[Claude Code's skills documentation](https://code.claude.com/docs/en/skills).
