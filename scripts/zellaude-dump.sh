#!/usr/bin/env bash
# zellaude-dump.sh — Dump the live plugin's internal state for debugging.
#
# Triggers the `zellaude:dump` pipe; every running instance (one per Zellij tab)
# writes one compact-JSON line to the Zellij log via stderr. This script reads
# those lines back, dedupes by plugin_id keeping the newest, and pretty-prints
# the result as a JSON array — one object per tab instance.
#
# Why the log and not stdout: a CLI pipe broadcasts to EVERY instance, and N
# writers on one pipe make cli_pipe_output race/loop. stderr→log is the robust
# path. See CLAUDE.md → Debugging.
#
# Usage: ./scripts/zellaude-dump.sh
#
# Read it: focus on flash_deadlines (compare to now_ms; 18446744073709551615 ==
# FlashMode::Persist, never expires), sessions[].activity (Waiting is not
# cleared by focusing), and per-instance acked_panes / focused_pane agreement.
set -euo pipefail

command -v jq >/dev/null 2>&1 || { echo "zellaude-dump: jq is required" >&2; exit 1; }

# Ask every instance to dump (broadcast). The plugin unblocks the pipe itself,
# so this returns immediately.
zellij pipe --name zellaude:dump

# Find the active session's log (newest mtime).
LOG=$(ls -t "${TMPDIR:-/tmp/}"zellij-*/zellij-log/zellij.log 2>/dev/null | head -1 || true)
if [ -z "$LOG" ]; then
    echo "zellaude-dump: could not find a zellij log under ${TMPDIR:-/tmp/}zellij-*" >&2
    exit 1
fi

# Give the async log write a beat to land.
sleep 0.2

grep 'zellaude-dump ' "$LOG" \
    | sed 's/.*zellaude-dump //' \
    | jq -s 'group_by(.plugin_id) | map(max_by(.now_ms))'
