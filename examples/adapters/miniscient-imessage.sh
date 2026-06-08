#!/bin/sh
set -eu

MINISCIENT_URL=${MINISCIENT_URL:-http://127.0.0.1:47873}
CHAT_DB=${CHAT_DB:-$HOME/Library/Messages/chat.db}
STATE_FILE=${STATE_FILE:-$HOME/.miniscient/imessage-adapter.state}
POLL_SECONDS=${POLL_SECONDS:-2}
TIMEOUT_SECONDS=${TIMEOUT_SECONDS:-300}
ALLOW_ALL=0
REPLAY_EXISTING=0
DRY_RUN=0
PRINT_ONLY=0
ALLOWED_HANDLES=""

usage() {
  cat <<'USAGE'
Usage: miniscient-imessage.sh [OPTIONS]

Poll macOS Messages for inbound iMessages, send allowed messages to a local
miniscient server, and reply via Messages.

Options:
  --miniscient-url URL   Default: http://127.0.0.1:47873
  --chat-db PATH         Default: ~/Library/Messages/chat.db
  --state-file PATH      Default: ~/.miniscient/imessage-adapter.state
  --poll-seconds N       Default: 2
  --timeout N            curl timeout for miniscient responses; default: 300
  --allow HANDLE         Allowed sender handle; repeatable
  --allow-all            Reply to every sender; unsafe outside testing
  --replay-existing      Process messages newer than saved state
  --dry-run              Poll and log but do not call miniscient
  --print-only           Call miniscient but print replies instead of sending
  -h, --help             Show this help

macOS permissions required for the process running this script:
  - Full Disk Access to read ~/Library/Messages/chat.db
  - Automation permission to control Messages via osascript
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --miniscient-url) MINISCIENT_URL=$2; shift 2 ;;
    --chat-db) CHAT_DB=$2; shift 2 ;;
    --state-file) STATE_FILE=$2; shift 2 ;;
    --poll-seconds) POLL_SECONDS=$2; shift 2 ;;
    --timeout) TIMEOUT_SECONDS=$2; shift 2 ;;
    --allow) ALLOWED_HANDLES=${ALLOWED_HANDLES}${ALLOWED_HANDLES:+"
"}$2; shift 2 ;;
    --allow-all) ALLOW_ALL=1; shift ;;
    --replay-existing) REPLAY_EXISTING=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --print-only) PRINT_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

need() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 1; }
}
need sqlite3
need curl
need jq
need perl
need osascript

normalize_handle() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | tr -d ' -().' 
}

is_allowed() {
  handle=$(normalize_handle "$1")
  [ "$ALLOW_ALL" = 1 ] && return 0
  printf '%s
' "$ALLOWED_HANDLES" | while IFS= read -r allowed; do
    [ "$(normalize_handle "$allowed")" = "$handle" ] && exit 0
  done
}

max_rowid() {
  sqlite3 -readonly "$CHAT_DB" 'SELECT COALESCE(MAX(ROWID), 0) FROM message;'
}

read_state() {
  if [ -f "$STATE_FILE" ]; then
    cat "$STATE_FILE"
  else
    printf '0'
  fi
}

write_state() {
  mkdir -p "$(dirname "$STATE_FILE")"
  printf '%s
' "$1" > "$STATE_FILE.tmp"
  mv "$STATE_FILE.tmp" "$STATE_FILE"
}

decode_hex() {
  perl -CS -e 'print pack("H*", $ARGV[0])' "$1"
}

send_imessage() {
  osascript - "$1" "$2" <<'APPLESCRIPT'
on run argv
  set targetHandle to item 1 of argv
  set replyText to item 2 of argv
  tell application "Messages"
    set targetService to 1st service whose service type = iMessage
    set targetBuddy to buddy targetHandle of targetService
    send replyText to targetBuddy
  end tell
end run
APPLESCRIPT
}

call_miniscient() {
  sender=$1
  incoming=$2
  prompt=$(printf 'Incoming iMessage from %s:\n\n%s\n\nReply with the text to send back to %s.' "$sender" "$incoming" "$sender")
  payload=$(jq -n --arg text "$prompt" --arg sender "$sender" \
    '{type:"message", text:$text, metadata:{source:"imessage", sender:$sender}}')
  curl -fsS --max-time "$TIMEOUT_SECONDS" \
    -H 'content-type: application/json' \
    -d "$payload" \
    "$MINISCIENT_URL/message" |
    jq -r 'if .ok then (.result.final_text // "") else error(.error // "miniscient request failed") end'
}

if [ ! -f "$CHAT_DB" ]; then
  echo "Messages database not found: $CHAT_DB" >&2
  exit 1
fi
if [ "$ALLOW_ALL" != 1 ] && [ -z "$ALLOWED_HANDLES" ]; then
  echo "refusing to run without --allow or --allow-all" >&2
  exit 1
fi

last_rowid=$(read_state)
case "$last_rowid" in *[!0-9]*|'') last_rowid=0 ;; esac
if [ "$REPLAY_EXISTING" != 1 ] && [ "$last_rowid" = 0 ]; then
  last_rowid=$(max_rowid)
  write_state "$last_rowid"
fi

echo "miniscient-imessage watching $CHAT_DB after rowid $last_rowid; server=$MINISCIENT_URL"

separator=$(printf '\037')
while :; do
  rows=$(sqlite3 -readonly -separator "$separator" "$CHAT_DB" \
    "SELECT message.ROWID, COALESCE(handle.id, ''), HEX(message.text)
       FROM message
       LEFT JOIN handle ON message.handle_id = handle.ROWID
      WHERE message.ROWID > $last_rowid
        AND message.is_from_me = 0
        AND message.text IS NOT NULL
        AND message.text != ''
      ORDER BY message.ROWID ASC;") || { echo "sqlite error; check Full Disk Access" >&2; sleep "$POLL_SECONDS"; continue; }

  if [ -n "$rows" ]; then
    printf '%s
' "$rows" | while IFS="$separator" read -r rowid handle hex_text; do
      [ -n "$rowid" ] || continue
      last_rowid=$rowid
      write_state "$last_rowid"
      text=$(decode_hex "$hex_text")

      if ! is_allowed "$handle"; then
        echo "skip $rowid from $handle: not allowed"
        continue
      fi

      echo "recv $rowid from $handle: $text"
      [ "$DRY_RUN" = 1 ] && continue

      if reply=$(call_miniscient "$handle" "$text"); then
        if [ -z "$reply" ]; then
          echo "skip $rowid: miniscient returned empty reply"
        elif [ "$PRINT_ONLY" = 1 ]; then
          echo "reply to $handle: $reply"
        else
          send_imessage "$handle" "$reply"
          echo "sent $rowid to $handle: $reply"
        fi
      else
        echo "error: miniscient request failed for row $rowid" >&2
      fi
    done
    last_rowid=$(read_state)
  fi
  sleep "$POLL_SECONDS"
done
