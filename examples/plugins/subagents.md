+++
id = "subagents"
title = "Shell Subagents"
type = "plugin"

[commands.mini_agent]
command = "mini"
required = true
reason = "The subagents plugin needs `mini` in PATH so the `subagents` helper can invoke it."

+++

# Shell Subagents

```bash install=subagents
#!/usr/bin/env bash
set -euo pipefail

ma="${MINI_BIN:-${MINI_AGENT_BIN:-mini}}"
home="${AGENT_HOME:?AGENT_HOME is not set}"
runs="$home/state/subagents/runs"
workspaces="$home/state/subagents/workspaces"

die() { echo "subagents: $*" >&2; exit 1; }
usage() { echo "usage: subagents [background|status|wait|show] [--name NAME] -- PROMPT..." >&2; }
stamp() { date "+subagent-%Y%m%d-%H%M%S-$$"; }
safe() { [[ "$1" =~ ^[A-Za-z0-9._-]+$ ]] || die "bad name: $1"; }
run_dir() { printf '%s/%s\n' "$runs" "$1"; }

parse() {
  name="$(stamp)"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --name) [[ $# -ge 2 ]] || die "--name needs a value"; name="$2"; safe "$name"; shift 2 ;;
      --) shift; break ;;
      -h|--help) usage; exit 0 ;;
      *) break ;;
    esac
  done
  [[ $# -gt 0 ]] || die "missing prompt"
  prompt="$*"
}

{% if plugins.jj.exists %}
workdir() {
  local root rel workspace="$workspaces/$name"
  if ! jj root >/dev/null 2>&1; then
    git_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not in a jj workspace"
    jj git init --colocate "$git_root" >/dev/null
  fi
  root="$(jj root)"
  [[ ! -e "$workspace" ]] || die "workspace exists: $workspace"
  mkdir -p "$workspaces"
  jj workspace add --name "$name" -m "subagent $name" "$workspace" >/dev/null
  [[ "$PWD" == "$root"/* ]] && rel="${PWD#"$root"/}" || rel=""
  [[ -n "$rel" && -d "$workspace/$rel" ]] && echo "$workspace/$rel" || echo "$workspace"
}
{% else %}
workdir() { echo "$PWD"; }
{% endif %}

run() {
  parse "$@"
  dir="$(workdir)"
  (cd "$dir" && exec "$ma" -p "$prompt")
}

background() {
  parse "$@"
  dir="$(run_dir "$name")"
  [[ ! -e "$dir" ]] || die "run exists: $name"
  mkdir -p "$dir"
  wd="$(workdir)"
  printf '%s\n' "$wd" > "$dir/workdir"
  printf '%s\n' "$prompt" > "$dir/prompt"
  (cd "$wd" && "$ma" -p "$prompt" > "$dir/out.txt" 2> "$dir/err.txt"; echo $? > "$dir/exit") &
  echo "$!" > "$dir/pid"
  printf 'name=%s\npid=%s\nout=%s\nerr=%s\nworkdir=%s\n' "$name" "$(cat "$dir/pid")" "$dir/out.txt" "$dir/err.txt" "$wd"
}

status() {
  dir="$(run_dir "${1:?missing run name}")"
  [[ -d "$dir" ]] || die "unknown run: $1"
  [[ -f "$dir/exit" ]] && { echo "done exit=$(cat "$dir/exit")"; return; }
  pid="$(cat "$dir/pid" 2>/dev/null || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    echo "running pid=$pid"
  else
    echo "exited without status"
    return 1
  fi
}

wait_run() {
  dir="$(run_dir "${1:?missing run name}")"
  [[ -d "$dir" ]] || die "unknown run: $1"
  while [[ ! -f "$dir/exit" ]]; do sleep 1; done
  status "$1"
  return "$(cat "$dir/exit")"
}

show() {
  dir="$(run_dir "${1:?missing run name}")"
  [[ -d "$dir" ]] || die "unknown run: $1"
  cat "$dir/out.txt"
  [[ ! -s "$dir/err.txt" ]] || echo "stderr=$dir/err.txt" >&2
}

cmd="${1:-run}"
case "$cmd" in
  run) shift; run "$@" ;;
  background) shift; background "$@" ;;
  status) shift; status "$@" ;;
  wait) shift; wait_run "$@" ;;
  show) shift; show "$@" ;;
  help|-h|--help) usage ;;
  *) run "$@" ;;
esac
```

You may start bounded subagent tasks by invoking `subagents` from the shell when
delegating a clear, narrow task would reduce latency or isolate investigation.

Use synchronous mode by default:

```bash
subagents -- "Inspect the parser and report the exact files involved."
```

Use background mode only when the parent can continue useful work while the
subagent runs:

```bash
subagents background --name parser-scan -- "Inspect the parser and report findings."
subagents status parser-scan
subagents wait parser-scan
subagents show parser-scan
```

{% if plugins.jj.exists %}
The helper automatically creates a separate `jj` workspace under
the agent state directory's `subagents/workspaces` tree for every subagent run, so background
editing work does not share the parent's working copy:

```bash
subagents --name docs-fix -- "Update the docs for the config change."
subagents background --name parser-fix -- "Fix the parser issue and summarize the jj change."
```

If the current directory is not already in a `jj` workspace, follow the jj plugin
initialization instructions before invoking subagents.
{% else %}
The helper runs subagents in the current working directory. Background subagents must be
read-only or otherwise isolated by a mechanism the user explicitly requested.
{% endif %}

Give each subagent a concrete task, an expected output shape, and any file or
directory boundaries. Do not ask a subagent to make broad, unbounded changes.
