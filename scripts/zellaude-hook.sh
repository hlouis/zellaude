#!/usr/bin/env bash
# zellaude-hook.sh — Claude Code hook → zellij pipe bridge
# Forwards hook events to the zellaude Zellij plugin via pipe.
#
# Usage in ~/.claude/settings.json hooks:
#   "command": "/path/to/zellaude-hook.sh"

# Exit silently if not running inside Zellij
[ -z "$ZELLIJ_SESSION_NAME" ] && exit 0
[ -z "$ZELLIJ_PANE_ID" ] && exit 0

# Capture send-time immediately so the plugin can order events
# that race through parallel hook subprocesses.
TS_MS=$(jq -nc 'now * 1000 | floor')

# Read hook JSON from stdin
INPUT=$(cat)

# Extract fields with jq (required dependency)
HOOK_EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty')
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty')
TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // empty')
CWD=$(echo "$INPUT" | jq -r '.cwd // empty')

[ -z "$HOOK_EVENT" ] && exit 0

# Build compact JSON payload
PAYLOAD=$(jq -nc \
  --arg pane_id "$ZELLIJ_PANE_ID" \
  --arg session_id "$SESSION_ID" \
  --arg hook_event "$HOOK_EVENT" \
  --arg tool_name "$TOOL_NAME" \
  --arg cwd "$CWD" \
  --arg zellij_session "$ZELLIJ_SESSION_NAME" \
  --arg term_program "${TERM_PROGRAM:-}" \
  --arg ts_ms "$TS_MS" \
  '{
    pane_id: ($pane_id | tonumber),
    session_id: $session_id,
    hook_event: $hook_event,
    tool_name: (if $tool_name == "" then null else $tool_name end),
    cwd: (if $cwd == "" then null else $cwd end),
    zellij_session: $zellij_session,
    term_program: (if $term_program == "" then null else $term_program end),
    ts_ms: ($ts_ms | tonumber)
  }')

# Desktop notification for events that need your attention.
# Which events notify — and their message — is decided here; everything
# below (focus check, rate-limit, delivery) is shared, no per-event branches.
#   PermissionRequest → blocking, loud (bell)
#   Stop              → main turn ended: work done / Claude is waiting on you
#                       (Claude Code can't tell "asking a question" from "finished")
# Title carries which session this is: the cwd's last path component
# (e.g. ".../ai/codebuddy" → "codebuddy"). The hook can't see the Zellij
# pane/tab name (that lives in the plugin), but cwd is right here and is the
# name users recognize. Falls back to "Claude Code" when cwd is absent.
PANE_NAME="Claude Code"
[ -n "$CWD" ] && PANE_NAME="$(basename "$CWD")"

NOTIFY_TITLE=""
NOTIFY_MESSAGE=""
BELL=false
case "$HOOK_EVENT" in
  PermissionRequest)
    BELL=true
    TOOL_SUFFIX=""
    [ -n "$TOOL_NAME" ] && TOOL_SUFFIX=" — $TOOL_NAME"
    NOTIFY_TITLE="⚠ $PANE_NAME"
    NOTIFY_MESSAGE="Permission requested${TOOL_SUFFIX}"
    ;;
  Stop)
    NOTIFY_TITLE="✅ $PANE_NAME"
    NOTIFY_MESSAGE="Done — your turn"
    ;;
esac

if [ -n "$NOTIFY_TITLE" ]; then
  [ "$BELL" = true ] && { printf '\a' > /dev/tty 2>/dev/null || true; }

  # Read notification setting (default: Always)
  SETTINGS_FILE="$HOME/.config/zellij/plugins/zellaude.json"
  NOTIFY_MODE="Always"
  if [ -f "$SETTINGS_FILE" ]; then
    NOTIFY_MODE=$(jq -r '.notifications // "Always"' "$SETTINGS_FILE" 2>/dev/null)
  fi

  # For "Unfocused" mode, decide whether you are actually looking at this pane.
  #
  # "Looking at it" is three nested layers, and a notification is pointless
  # only if all three say yes:
  #   1. the Zellij tab holding this pane is the one on screen  (plugin file)
  #   2. the terminal tab/window showing this session is on top (window title)
  #   3. the terminal app is frontmost                          (WM/OS)
  # Each check can only ever *reject* ("you are not looking at it" → notify),
  # and a check whose evidence is unavailable is skipped rather than guessed.
  # That keeps the failure mode on the safe side: a missing signal means one
  # notification too many, never a swallowed one.
  #
  # Not covered on purpose: which *pane* inside the Zellij tab has the cursor.
  # If the tab is on screen the status bar's ▶/flash is right there in view —
  # a desktop notification would be redundant.
  SHOULD_NOTIFY=false
  case "$NOTIFY_MODE" in
    Always) SHOULD_NOTIFY=true ;;
    Unfocused)
      TERM_FOCUSED=true

      # (1) Is this pane in the Zellij tab currently on screen? The plugin
      # publishes the on-screen tab's pane ids per session; we only know our
      # own $ZELLIJ_PANE_ID. Cheapest check, and the one that fires most —
      # do it first so the common "background tab finished" case never pays
      # for an osascript round-trip. No file (plugin missing/too old) → skip.
      FOCUS_FILE="/tmp/zellaude-focus-${ZELLIJ_SESSION_NAME//[^a-zA-Z0-9_-]/_}.json"
      if [ -f "$FOCUS_FILE" ]; then
        jq -e --argjson p "$ZELLIJ_PANE_ID" \
          '.active_panes | index($p) != null' "$FOCUS_FILE" >/dev/null 2>&1 \
          || TERM_FOCUSED=false
      fi

      # Layers (2) and (3) can only confirm what (1) already rejected, so skip
      # them — and their osascript round-trip — once (1) has said no. "skip"
      # falls into the same catch-all branch as an unsupported OS, which sets
      # TERM_FOCUSED=false: already false, so no special case is needed.
      OS=$(uname)
      [ "$TERM_FOCUSED" = false ] && OS=skip
      case "$OS" in
        Darwin)
          # Map TERM_PROGRAM to macOS process name
          EXPECTED="${TERM_PROGRAM:-}"
          case "$EXPECTED" in
            Apple_Terminal) EXPECTED="Terminal" ;;
            iTerm.app)     EXPECTED="iTerm2" ;;
          esac
          FRONT_APP=$(osascript -e 'tell application "System Events" to get name of first application process whose frontmost is true' 2>/dev/null)
          [ "$FRONT_APP" != "$EXPECTED" ] && TERM_FOCUSED=false

          # (2) One terminal app, many native tabs — often one Zellij session
          # each. Ask which tab is on top and match it against this session,
          # same "title starts with the session name" convention the
          # notification-click raise below relies on. Ghostty only: it is the
          # one that ships an AppleScript dictionary (System Events' generic
          # `window 1 of process` is unreliable here — returns "invalid index"
          # depending on window state). Empty/failed title → skip the check.
          if [ "$TERM_FOCUSED" = true ] && [ "${TERM_PROGRAM:-}" = "ghostty" ]; then
            FRONT_TITLE=$(osascript -e 'tell application "Ghostty" to get name of front window' 2>/dev/null)
            case "$FRONT_TITLE" in
              "") ;;
              "$ZELLIJ_SESSION_NAME"*) ;;
              *) TERM_FOCUSED=false ;;
            esac
          fi
          ;;
        Linux)
          # X11: check if focused window belongs to our terminal
          TERM_FOCUSED=false
          if command -v xdotool >/dev/null 2>&1; then
            ACTIVE_PID=$(xdotool getactivewindow getwindowpid 2>/dev/null)
            if [ -n "$ACTIVE_PID" ]; then
              # Walk up the process tree from our shell to see if the
              # focused window's process is an ancestor (i.e. our terminal)
              PID=$$
              while [ "$PID" -gt 1 ] 2>/dev/null; do
                [ "$PID" = "$ACTIVE_PID" ] && { TERM_FOCUSED=true; break; }
                PID=$(ps -o ppid= -p "$PID" 2>/dev/null | tr -d ' ')
              done
            fi
            # (2) Same tab-title check as macOS: one terminal process can host
            # several tabs/windows, so the ancestor walk above is app-level.
            if [ "$TERM_FOCUSED" = true ]; then
              FRONT_TITLE=$(xdotool getactivewindow getwindowname 2>/dev/null)
              case "$FRONT_TITLE" in
                "") ;;
                "$ZELLIJ_SESSION_NAME"*) ;;
                *) TERM_FOCUSED=false ;;
              esac
            fi
          fi
          # Wayland: no standard way to check; fall through to not-focused
          ;;
        *) TERM_FOCUSED=false ;;
      esac

      [ "$TERM_FOCUSED" = false ] && SHOULD_NOTIFY=true
      ;;
  esac

  if [ "$SHOULD_NOTIFY" = true ]; then
    TITLE="$NOTIFY_TITLE"
    MESSAGE="$NOTIFY_MESSAGE"

    # Rate-limit: one notification per pane per event type per 10 seconds.
    # Pane ids are per-session, so the lock must be session-qualified —
    # otherwise pane 5 in two sessions would share a lock and silently
    # swallow each other's notifications. Qualifying by event too keeps a
    # Stop from suppressing a PermissionRequest that lands right after it.
    LOCK="/tmp/zellaude-notify-${ZELLIJ_SESSION_NAME//[^a-zA-Z0-9_-]/_}-${ZELLIJ_PANE_ID}-${HOOK_EVENT}"
    NOW=$(date +%s)
    LAST=0
    [ -f "$LOCK" ] && LAST=$(cat "$LOCK" 2>/dev/null)
    if [ $((NOW - LAST)) -ge 10 ]; then
      echo "$NOW" > "$LOCK"

      # Click callback: raise the terminal (to the right tab) + focus the pane.
      ZELLIJ_BIN=$(command -v zellij)
      FOCUS_CMD="${ZELLIJ_BIN} -s '${ZELLIJ_SESSION_NAME}' pipe --name zellaude:focus -- ${ZELLIJ_PANE_ID}"

      case "$(uname)" in
        Darwin)
          # `open -a` only brings the app forward — it can't pick a native tab.
          # Ghostty 1.3+ ships an AppleScript dictionary, so for Ghostty we
          # select the tab whose title starts with this Zellij session name
          # (Ghostty surfaces the OSC title as `name of tab`, e.g.
          # "Util | <pane title>"). That lands the click on the right macOS tab;
          # the zellij pipe then focuses the right pane inside it. Other
          # terminals fall back to `open -a` (app-level raise only).
          if [ "${TERM_PROGRAM:-}" = "ghostty" ]; then
            RAISE_CMD="osascript -e 'tell application \"Ghostty\"' -e 'repeat with w in windows' -e 'repeat with t in tabs of w' -e 'if (name of t) starts with \"${ZELLIJ_SESSION_NAME}\" then' -e 'select tab t' -e 'activate window w' -e 'focus (focused terminal of t)' -e 'end if' -e 'end repeat' -e 'end repeat' -e 'end tell'"
          elif [ -n "${TERM_PROGRAM:-}" ]; then
            RAISE_CMD="open -a '${TERM_PROGRAM}'"
          else
            RAISE_CMD=":"
          fi
          # `;` not `&&`: focus the pane even if the raise fails (e.g. TCC not
          # yet granted), so pane focus never depends on tab selection.
          FOCUS_CMD="${RAISE_CMD}; ${FOCUS_CMD}"
          if command -v terminal-notifier >/dev/null 2>&1; then
            terminal-notifier \
              -title "$TITLE" \
              -message "$MESSAGE" \
              -execute "$FOCUS_CMD" &
          else
            osascript -e "display notification \"$MESSAGE\" with title \"$TITLE\"" &
          fi
          ;;
        Linux)
          if command -v notify-send >/dev/null 2>&1; then
            notify-send "$TITLE" "$MESSAGE" &
          fi
          ;;
      esac
    fi
  fi
fi

# Send to plugin (hook is already async, no need to background)
zellij pipe --name "zellaude" -- "$PAYLOAD"
