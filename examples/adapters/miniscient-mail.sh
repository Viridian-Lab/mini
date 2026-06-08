#!/usr/bin/env bash
set -euo pipefail

MINISCIENT_URL=${MINISCIENT_URL:-http://127.0.0.1:47873}
STATE_FILE=${STATE_FILE:-$HOME/.miniscient/mail-adapter.state}
POLL_SECONDS=${POLL_SECONDS:-10}
TIMEOUT_SECONDS=${TIMEOUT_SECONDS:-300}
ALLOW_ALL=0
DRY_RUN=0
PRINT_ONLY=0
ALLOWED_SENDERS=""

usage() {
  cat <<'USAGE'
Usage: miniscient-mail.sh [OPTIONS]

Poll Apple Mail for unread inbox messages, send allowed messages to a local
miniscient server, and reply to the original email thread through Mail.

Options:
  --miniscient-url URL   Default: http://127.0.0.1:47873
  --state-file PATH      Default: ~/.miniscient/mail-adapter.state
  --poll-seconds N       Default: 10
  --timeout N            curl timeout for miniscient responses; default: 300
  --allow SENDER         Allowed sender substring/email; repeatable
  --allow-all            Reply to every unread sender; unsafe outside testing
  --dry-run              Poll and log but do not call miniscient
  --print-only           Call miniscient but print replies instead of sending
  -h, --help             Show this help

Requires Apple Mail to have the target account configured, plus Automation
permission for the process running this script to control Mail.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --miniscient-url) MINISCIENT_URL=$2; shift 2 ;;
    --state-file) STATE_FILE=$2; shift 2 ;;
    --poll-seconds) POLL_SECONDS=$2; shift 2 ;;
    --timeout) TIMEOUT_SECONDS=$2; shift 2 ;;
    --allow) ALLOWED_SENDERS=${ALLOWED_SENDERS}${ALLOWED_SENDERS:+$'\n'}$2; shift 2 ;;
    --allow-all) ALLOW_ALL=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --print-only) PRINT_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 1; }; }
need osascript
need curl
need jq

if [[ "$ALLOW_ALL" != 1 && -z "$ALLOWED_SENDERS" ]]; then
  echo "refusing to run without --allow or --allow-all" >&2
  exit 1
fi

mkdir -p "$(dirname "$STATE_FILE")"
touch "$STATE_FILE"

is_allowed() {
  local sender_lc allowed_lc
  sender_lc=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
  [[ "$ALLOW_ALL" = 1 ]] && return 0
  while IFS= read -r allowed; do
    [[ -z "$allowed" ]] && continue
    allowed_lc=$(printf '%s' "$allowed" | tr '[:upper:]' '[:lower:]')
    [[ "$sender_lc" == *"$allowed_lc"* ]] && return 0
  done <<< "$ALLOWED_SENDERS"
  return 1
}

seen() { grep -Fxq "$1" "$STATE_FILE"; }
mark_seen() { printf '%s\n' "$1" >> "$STATE_FILE"; }

fetch_unread() {
  osascript <<'APPLESCRIPT'
set rs to ASCII character 30
set fs to ASCII character 31
set out to ""
tell application "Mail"
  try
    set unreadMessages to messages of inbox whose read status is false
    repeat with m in unreadMessages
      set mid to id of m as text
      set senderText to sender of m as text
      set subjectText to subject of m as text
      set contentText to content of m as text
      set out to out & mid & fs & senderText & fs & subjectText & fs & contentText & rs
    end repeat
  on error errMsg
    return "ERROR" & fs & errMsg
  end try
end tell
return out
APPLESCRIPT
}

reply_mail() {
  local message_id=$1 reply=$2
  osascript - "$message_id" "$reply" <<'APPLESCRIPT'
on run argv
  set targetId to item 1 of argv
  set replyText to item 2 of argv
  tell application "Mail"
    set targetMessage to missing value
    repeat with m in messages of inbox
      if (id of m as text) is targetId then
        set targetMessage to m
        exit repeat
      end if
    end repeat
    if targetMessage is missing value then error "message not found: " & targetId
    set outgoing to reply targetMessage opening window false
    set content of outgoing to replyText & return & return & content of outgoing
    send outgoing
    set read status of targetMessage to true
  end tell
end run
APPLESCRIPT
}

call_miniscient() {
  local sender=$1 subject=$2 body=$3 prompt payload
  prompt=$(printf 'Incoming email from %s\nSubject: %s\n\n%s\n\nReply with the email body to send back.' "$sender" "$subject" "$body")
  payload=$(jq -n --arg text "$prompt" --arg sender "$sender" --arg subject "$subject" \
    '{type:"message", text:$text, metadata:{source:"email", sender:$sender, subject:$subject}}')
  curl -fsS --max-time "$TIMEOUT_SECONDS" \
    -H 'content-type: application/json' \
    -d "$payload" \
    "$MINISCIENT_URL/message" |
    jq -r 'if .ok then (.result.final_text // "") else error(.error // "miniscient request failed") end'
}

echo "miniscient-mail watching Apple Mail unread inbox; server=$MINISCIENT_URL"
rs=$(printf '\036')
fs=$(printf '\037')
while :; do
  data=$(fetch_unread || true)
  if [[ "$data" == ERROR$fs* ]]; then
    echo "mail error: ${data#ERROR$fs}" >&2
    sleep "$POLL_SECONDS"
    continue
  fi
  if [[ -n "$data" ]]; then
    while IFS="$fs" read -r message_id sender subject body; do
      [[ -n "$message_id" ]] || continue
      if seen "$message_id"; then continue; fi
      mark_seen "$message_id"
      if ! is_allowed "$sender"; then
        echo "skip $message_id from $sender: not allowed"
        continue
      fi
      echo "recv $message_id from $sender: $subject"
      [[ "$DRY_RUN" = 1 ]] && continue
      if reply=$(call_miniscient "$sender" "$subject" "$body"); then
        if [[ -z "$reply" ]]; then
          echo "skip $message_id: miniscient returned empty reply"
        elif [[ "$PRINT_ONLY" = 1 ]]; then
          echo "reply to $sender: $reply"
        else
          reply_mail "$message_id" "$reply"
          echo "sent reply to $sender for $message_id"
        fi
      else
        echo "error: miniscient request failed for message $message_id" >&2
      fi
    done < <(printf '%s' "$data" | tr "$rs" '\n')
  fi
  sleep "$POLL_SECONDS"
done
