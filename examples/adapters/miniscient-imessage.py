#!/usr/bin/env python3
"""Bridge incoming iMessages to a local miniscient server.

This adapter intentionally runs out-of-process. It polls the local Messages
SQLite database for new inbound messages, sends allowed messages to miniscient's
`/message` endpoint, then replies through Messages with AppleScript.

macOS permissions required:
- Full Disk Access for the process that runs this script, so it can read
  ~/Library/Messages/chat.db.
- Automation permission to let that process control Messages via osascript.
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

DEFAULT_MINISCIENT_URL = "http://127.0.0.1:47873"
DEFAULT_CHAT_DB = "~/Library/Messages/chat.db"
DEFAULT_STATE_FILE = "~/.miniscient/imessage-adapter.json"


@dataclass(frozen=True)
class IncomingMessage:
    rowid: int
    guid: str | None
    handle: str
    text: str


def normalize_handle(handle: str) -> str:
    return "".join(ch for ch in handle.strip().lower() if ch not in " -().")


def load_state(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    with path.open("r", encoding="utf-8") as file:
        return json.load(file)


def save_state(path: Path, state: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as file:
        json.dump(state, file, indent=2, sort_keys=True)
        file.write("\n")
    tmp.replace(path)


def connect_messages_db(path: Path) -> sqlite3.Connection:
    # mode=ro avoids creating sidecar files and works with the Messages WAL.
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def max_rowid(chat_db: Path) -> int:
    with connect_messages_db(chat_db) as db:
        row = db.execute("SELECT COALESCE(MAX(ROWID), 0) FROM message").fetchone()
        return int(row[0] or 0)


def incoming_messages(chat_db: Path, after_rowid: int) -> list[IncomingMessage]:
    with connect_messages_db(chat_db) as db:
        rows = db.execute(
            """
            SELECT message.ROWID, message.guid, handle.id, message.text
            FROM message
            LEFT JOIN handle ON message.handle_id = handle.ROWID
            WHERE message.ROWID > ?
              AND message.is_from_me = 0
              AND message.text IS NOT NULL
              AND message.text != ''
            ORDER BY message.ROWID ASC
            """,
            (after_rowid,),
        ).fetchall()
    messages = []
    for rowid, guid, handle, text in rows:
        if not handle:
            continue
        messages.append(
            IncomingMessage(
                rowid=int(rowid),
                guid=guid,
                handle=str(handle),
                text=str(text),
            )
        )
    return messages


def miniscient_message(base_url: str, sender: str, text: str, timeout: float) -> str:
    payload = {
        "type": "message",
        "text": f"Incoming iMessage from {sender}:\n\n{text}\n\nReply with the text to send back to {sender}.",
        "metadata": {"source": "imessage", "sender": sender},
    }
    request = urllib.request.Request(
        base_url.rstrip("/") + "/message",
        data=json.dumps(payload).encode("utf-8"),
        headers={"content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            envelope = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        body = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"miniscient returned HTTP {err.code}: {body}") from err

    if not envelope.get("ok"):
        raise RuntimeError(envelope.get("error") or "miniscient request failed")
    result = envelope.get("result") or {}
    return str(result.get("final_text") or "").strip()


def send_imessage(handle: str, text: str) -> None:
    script = """
on run argv
  set targetHandle to item 1 of argv
  set replyText to item 2 of argv
  tell application "Messages"
    set targetService to 1st service whose service type = iMessage
    set targetBuddy to buddy targetHandle of targetService
    send replyText to targetBuddy
  end tell
end run
""".strip()
    subprocess.run(
        ["osascript", "-e", script, handle, text],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def should_process(handle: str, allowed: set[str], allow_all: bool) -> bool:
    return allow_all or normalize_handle(handle) in allowed


def run(args: argparse.Namespace) -> int:
    chat_db = Path(args.chat_db).expanduser()
    state_file = Path(args.state_file).expanduser()
    if not chat_db.exists():
        raise SystemExit(f"Messages database not found: {chat_db}")

    allowed = {normalize_handle(handle) for handle in args.allow}
    if not args.allow_all and not allowed:
        raise SystemExit("refusing to run without --allow or --allow-all")

    state = load_state(state_file)
    if args.replay_existing:
        last_rowid = int(state.get("last_rowid", 0))
    else:
        last_rowid = int(state.get("last_rowid") or max_rowid(chat_db))
        state["last_rowid"] = last_rowid
        save_state(state_file, state)

    print(
        f"miniscient-imessage watching {chat_db} after rowid {last_rowid}; "
        f"server={args.miniscient_url}",
        flush=True,
    )

    while True:
        try:
            for message in incoming_messages(chat_db, last_rowid):
                last_rowid = max(last_rowid, message.rowid)
                state["last_rowid"] = last_rowid
                save_state(state_file, state)

                if not should_process(message.handle, allowed, args.allow_all):
                    print(f"skip {message.rowid} from {message.handle}: not allowed", flush=True)
                    continue

                print(f"recv {message.rowid} from {message.handle}: {message.text!r}", flush=True)
                if args.dry_run:
                    continue

                reply = miniscient_message(
                    args.miniscient_url,
                    message.handle,
                    message.text,
                    args.timeout,
                )
                if not reply:
                    print(f"skip {message.rowid}: miniscient returned empty reply", flush=True)
                    continue

                if args.print_only:
                    print(f"reply to {message.handle}: {reply}", flush=True)
                else:
                    send_imessage(message.handle, reply)
                    print(f"sent {message.rowid} to {message.handle}: {reply!r}", flush=True)
        except KeyboardInterrupt:
            return 0
        except Exception as err:  # keep the adapter alive across transient failures
            print(f"error: {err}", file=sys.stderr, flush=True)

        time.sleep(args.poll_seconds)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Bridge iMessage to miniscient")
    parser.add_argument("--miniscient-url", default=os.environ.get("MINISCIENT_URL", DEFAULT_MINISCIENT_URL))
    parser.add_argument("--chat-db", default=DEFAULT_CHAT_DB)
    parser.add_argument("--state-file", default=DEFAULT_STATE_FILE)
    parser.add_argument("--poll-seconds", type=float, default=2.0)
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument("--allow", action="append", default=[], help="Allowed sender handle; repeatable")
    parser.add_argument("--allow-all", action="store_true", help="Reply to every sender; unsafe outside testing")
    parser.add_argument("--replay-existing", action="store_true", help="Process messages already newer than saved state")
    parser.add_argument("--dry-run", action="store_true", help="Poll and log but do not call miniscient")
    parser.add_argument("--print-only", action="store_true", help="Call miniscient but print replies instead of sending")
    return parser.parse_args()


if __name__ == "__main__":
    raise SystemExit(run(parse_args()))
