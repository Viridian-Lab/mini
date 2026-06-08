+++
id = "macos-control"
title = "macOS Computer Control"
type = "plugin"

[commands.osascript]
command = "osascript"
required = true
reason = "macOS UI automation needs osascript."

[commands.screencapture]
command = "screencapture"
required = false
reason = "Install/enable macOS screencapture for screenshot capture."
+++

# macOS Computer Control

You may use the installed macOS helper commands to inspect and operate the local
Mac when the user asks you to manage the computer, accounts, browser, Mail, or
other apps. Prefer text-level inspection (`macos-frontmost`, `macos-apps`,
`macos-ui`) before clicking. Use small reversible steps, and report what you did.

For sensitive, financial, security, legal, or account-destructive actions, pause
and ask for confirmation unless the user has already explicitly authorized that
specific action. Do not ask the user to paste passwords into chat; instead use
existing browser sessions, Mail, Keychain, or password-manager UI already present
on the Mac.

Useful commands:

```bash
macos-apps                         # list visible running apps
macos-frontmost                    # show frontmost app and window names
macos-activate "Safari"            # bring an app to front
macos-ui [app] [depth]              # dump front app accessibility tree
macos-click <x> <y>                 # click screen coordinates
macos-type <text>                   # type text into focused UI
macos-key <key> [modifiers...]      # send a keystroke, e.g. macos-key l command
macos-open <url-or-path>            # open URL/path/app with macOS open
macos-screenshot [path]             # capture screenshot and print path
```

The computer may need macOS Privacy permissions for the process running you:
Accessibility for UI inspection/click/type, Screen Recording for screenshots,
and Automation for controlling apps.

```bash install=macos-apps
#!/usr/bin/env bash
set -euo pipefail
osascript <<'APPLESCRIPT'
tell application "System Events"
  set appNames to name of every process whose background only is false
end tell
set text item delimiters to linefeed
return appNames as text
APPLESCRIPT
```

```bash install=macos-frontmost
#!/usr/bin/env bash
set -euo pipefail
osascript <<'APPLESCRIPT'
tell application "System Events"
  set frontProc to first process whose frontmost is true
  set appName to name of frontProc
  set out to "frontmost: " & appName
  try
    repeat with w in windows of frontProc
      set out to out & linefeed & "window: " & (name of w as text)
    end repeat
  end try
  return out
end tell
APPLESCRIPT
```

```bash install=macos-activate
#!/usr/bin/env bash
set -euo pipefail
app="${1:?usage: macos-activate APP_NAME}"
osascript - "$app" <<'APPLESCRIPT'
on run argv
  tell application (item 1 of argv) to activate
end run
APPLESCRIPT
```

```bash install=macos-ui
#!/usr/bin/env bash
set -euo pipefail
app="${1:-}"
depth="${2:-2}"
if [[ -z "$app" ]]; then
  app="$(osascript -e 'tell application "System Events" to get name of first process whose frontmost is true')"
fi
osascript - "$app" "$depth" <<'APPLESCRIPT'
on attrText(e, attrName)
  try
    set v to value of attribute attrName of e
    return v as text
  on error
    return ""
  end try
end attrText

on describeElement(e, indent)
  set roleText to my attrText(e, "AXRole")
  set subroleText to my attrText(e, "AXSubrole")
  set titleText to my attrText(e, "AXTitle")
  set descText to my attrText(e, "AXDescription")
  set valueText to my attrText(e, "AXValue")
  set posText to my attrText(e, "AXPosition")
  set sizeText to my attrText(e, "AXSize")
  set pieces to {}
  if roleText is not "" then set end of pieces to roleText
  if subroleText is not "" then set end of pieces to "subrole=" & subroleText
  if titleText is not "" then set end of pieces to "title=" & titleText
  if descText is not "" then set end of pieces to "description=" & descText
  if valueText is not "" then set end of pieces to "value=" & valueText
  if posText is not "" then set end of pieces to "pos=" & posText
  if sizeText is not "" then set end of pieces to "size=" & sizeText
  set AppleScript's text item delimiters to " | "
  set lineText to pieces as text
  set AppleScript's text item delimiters to ""
  return indent & lineText
end describeElement

on dumpElement(e, indent, depthLeft)
  set out to my describeElement(e, indent) & linefeed
  if depthLeft <= 0 then return out
  try
    set kids to UI elements of e
    repeat with child in kids
      set out to out & my dumpElement(child, indent & "  ", depthLeft - 1)
    end repeat
  end try
  return out
end dumpElement

on run argv
  set appName to item 1 of argv
  set maxDepth to (item 2 of argv) as integer
  tell application "System Events"
    tell process appName
      set out to "app: " & appName & linefeed
      try
        repeat with w in windows
          set out to out & my dumpElement(w, "", maxDepth)
        end repeat
      on error errMsg
        set out to out & "error: " & errMsg
      end try
      return out
    end tell
  end tell
end run
APPLESCRIPT
```

```bash install=macos-click
#!/usr/bin/env bash
set -euo pipefail
[[ $# -eq 2 ]] || { echo "usage: macos-click X Y" >&2; exit 2; }
osascript - "$1" "$2" <<'APPLESCRIPT'
on run argv
  set x to (item 1 of argv) as integer
  set y to (item 2 of argv) as integer
  tell application "System Events" to click at {x, y}
end run
APPLESCRIPT
```

```bash install=macos-type
#!/usr/bin/env bash
set -euo pipefail
text="$*"
[[ -n "$text" ]] || { echo "usage: macos-type TEXT" >&2; exit 2; }
osascript - "$text" <<'APPLESCRIPT'
on run argv
  tell application "System Events" to keystroke (item 1 of argv)
end run
APPLESCRIPT
```

```bash install=macos-key
#!/usr/bin/env bash
set -euo pipefail
[[ $# -ge 1 ]] || { echo "usage: macos-key KEY [command] [option] [shift] [control]" >&2; exit 2; }
key="$1"; shift || true
mods="$(printf '%s,' "$@")"
osascript - "$key" "$mods" <<'APPLESCRIPT'
on run argv
  set keyName to item 1 of argv
  set modText to item 2 of argv
  set modifierList to {}
  if modText contains "command," then copy command down to end of modifierList
  if modText contains "cmd," then copy command down to end of modifierList
  if modText contains "option," then copy option down to end of modifierList
  if modText contains "alt," then copy option down to end of modifierList
  if modText contains "shift," then copy shift down to end of modifierList
  if modText contains "control," then copy control down to end of modifierList
  if modText contains "ctrl," then copy control down to end of modifierList
  tell application "System Events"
    if (count of modifierList) is 0 then
      keystroke keyName
    else
      keystroke keyName using modifierList
    end if
  end tell
end run
APPLESCRIPT
```

```bash install=macos-open
#!/usr/bin/env bash
set -euo pipefail
[[ $# -ge 1 ]] || { echo "usage: macos-open URL_OR_PATH..." >&2; exit 2; }
open "$@"
```

```bash install=macos-screenshot
#!/usr/bin/env bash
set -euo pipefail
path="${1:-${MINI_AGENT_HOME:-$HOME/.mini-agent}/state/screenshots/screenshot-$(date +%Y%m%d-%H%M%S).png}"
mkdir -p "$(dirname "$path")"
screencapture -x "$path"
printf '%s\n' "$path"
```
