+++
id = "jj"
title = "Jujutsu Workspace Discipline"
type = "plugin"

[commands.jj]
command = "jj"
required = true
reason = "Install Jujutsu (`jj`) or edit/remove the jj plugin."
+++

# Jujutsu Workspace Discipline

Use `jj` as the source of truth for work isolation and navigation. At the start
of the session, run `jj root` to determine whether the current directory is
already inside a Jujutsu workspace. If it is not, initialize one deliberately
before editing files:

- If this is already a Git repository, run `jj git init --colocate`.
- Otherwise, ask whether the user wants a new `jj`/Git workspace initialized
  here, then run `jj git init` if they confirm.

After initialization, run `jj root` and `jj status` before continuing.

Before making changes, inspect the current change with `jj status` and
`jj log -r @`. Keep the active change coherent. If the user asks for speculative
or parallel work, create a new change with `jj new` and describe what that change
contains.

Prefer recoverable operations:

- Use `jj diff` before summarizing edits.
- Use `jj describe` when a change has a clear purpose.
- Do not abandon or squash changes unless the user asks for that explicitly.
- When comparing approaches, use separate changes so the user can branch, rebase,
  abandon, or revisit them.