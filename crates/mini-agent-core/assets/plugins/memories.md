+++
id = "memories"
title = "Memories"
type = "plugin"
+++

# Memories

```bash install=memories
#!/usr/bin/env bash
set -euo pipefail

root=".mini/memories"

usage() {
  cat >&2 <<'USAGE'
usage:
  memories add <path> <content...>
  memories show <path>
  memories ls [path]
  memories rm <path>

Memories are markdown files under .mini/memories. Paths are slash-separated,
relative names without '.', '..', empty components, leading '/', or backslashes.
The .md extension is optional.
USAGE
}

die() { echo "memories: $*" >&2; exit 1; }

validate_relpath() {
  local path="$1"
  [[ -n "$path" ]] || die "missing path"
  [[ "$path" != /* ]] || die "absolute paths are not allowed: $path"
  [[ "$path" != *\\* ]] || die "backslashes are not allowed: $path"
  [[ "$path" != *//* ]] || die "empty path components are not allowed: $path"
  IFS='/' read -r -a parts <<< "$path"
  for part in "${parts[@]}"; do
    [[ -n "$part" && "$part" != "." && "$part" != ".." ]] || die "invalid path component in: $path"
  done
}

memory_file() {
  local path="$1"
  validate_relpath "$path"
  [[ "$path" == *.md ]] || path="$path.md"
  printf '%s/%s\n' "$root" "$path"
}

write_memory() {
  local path="$1" content="$2" file
  file="$(memory_file "$path")"
  mkdir -p "$(dirname "$file")"
  printf '%s\n' "$content" > "$file"
  printf 'saved %s\n' "${file#./}"
}

cmd_add() {
  [[ $# -ge 2 ]] || die "add needs <path> and <content>"
  local path="$1" content
  shift
  content="$*"
  write_memory "$path" "$content"
}
cmd_show() {
  [[ $# -eq 1 ]] || die "show needs exactly one <path>"
  local file
  file="$(memory_file "$1")"
  [[ -f "$file" ]] || die "not found: $1"
  cat "$file"
}

print_tree() {
  awk -F/ '
    {
      path = ""
      for (i = 1; i <= NF; i++) {
        if (i == NF) {
          for (j = 1; j < i; j++) printf "  "
          print $i
        } else {
          path = path $i "/"
          if (!seen[path]) {
            seen[path] = 1
            for (j = 1; j < i; j++) printf "  "
            print $i "/"
          }
        }
      }
    }
  '
}
cmd_ls() {
  local base="$root" rel="${1:-}"
  if [[ -n "$rel" ]]; then
    validate_relpath "$rel"
    base="$root/$rel"
    [[ -e "$base" || -e "$base.md" ]] || { echo "(no memories)"; return; }
    [[ -e "$base" ]] || base="$base.md"
  fi
  [[ -e "$base" ]] || { echo "(no memories)"; return; }
  if [[ -f "$base" ]]; then
    printf '%s\n' "${base#"$root/"}"
    return
  fi
  local files
  files="$(find "$base" -type f -name '*.md' | sort | sed "s#^$root/##")"
  [[ -n "$files" ]] || { echo "(no memories)"; return; }
  printf '%s\n' "$files" | print_tree
}
cmd_rm() {
  [[ $# -eq 1 ]] || die "rm needs exactly one <path>"
  local file dir
  file="$(memory_file "$1")"
  [[ -f "$file" ]] || die "not found: $1"
  rm "$file"
  dir="$(dirname "$file")"
  while [[ "$dir" != "$root" && "$dir" == "$root"/* ]]; do
    rmdir "$dir" 2>/dev/null || break
    dir="$(dirname "$dir")"
  done
  printf 'removed %s\n' "${file#./}"
}

cmd="${1:-ls}"
case "$cmd" in
  add|set|write) shift; cmd_add "$@" ;;
  show|cat|read) shift; cmd_show "$@" ;;
  ls|list|tree) shift; cmd_ls "$@" ;;
  rm|remove|delete) shift; cmd_rm "$@" ;;
  help|-h|--help) usage ;;
  *) usage; die "unknown command: $cmd" ;;
esac
```

Use the `memories` helper to maintain per-project persistent memories.

Available commands:

```bash
memories add <path> <content...>
memories show <path>
memories ls [path]
memories rm <path>
```

Use memories for things you should remember. 