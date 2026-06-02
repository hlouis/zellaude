# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Zellaude is a Zellij status-bar plugin (compiled to WASM) that replaces the native tab bar with a view that shows what every Claude Code session is doing. It pairs the WASM plugin with a thin bash hook script that bridges Claude Code hook events into the plugin.

## Build & install

```bash
./install.sh              # build WASM, copy to ~/.config/zellij/plugins/, register hooks
./install.sh --uninstall  # remove plugin + hook entries
cargo build --release     # build only (target wasm32-wasip1 is set in .cargo/config.toml)
zellij --layout layout.kdl # run with the bundled layout
```

There are no tests. Build target is `wasm32-wasip1` (added automatically by `install.sh` via rustup). `jq` is a runtime dependency of the hook script.

After changing Rust code, rebuild and copy the wasm, then reload the plugin in Zellij (or restart the session) to see changes. The hook script and its `~/.claude/settings.json` registration are re-installed automatically by the plugin on load (see Bootstrapping below) — you do not normally run `scripts/install-hooks.sh` by hand.

## Architecture

The data flow is one-directional:

```
Claude Code hook → zellaude-hook.sh → `zellij pipe` → plugin.pipe() → State → render()
```

**All state lives in WASM memory** (`State` in `src/state.rs`). There are no temp files or shared databases for session state. The plugin runs as **one instance per Zellij tab**, and instances keep a unified view by message-passing over `pipe_message_to_plugin` (see Multi-instance sync).

### Files

- `src/main.rs` — the `ZellijPlugin` impl: `load`, `update` (Zellij events), `pipe` (hook + inter-plugin messages), `render`. Also holds `State` helper methods (sync, config I/O, stale cleanup, flash cleanup). Event subscriptions and permission requests are set up in `load`.
- `src/state.rs` — `State` struct, `SessionInfo`, the `Activity` enum (the core state machine), `Settings`, and the click-region / menu types. `HookPayload` is the deserialized hook event.
- `src/event_handler.rs` — `handle_hook_event`: maps an incoming hook event onto a session's `Activity` and timestamps. The single place where hook semantics turn into state.
- `src/render.rs` — builds the status bar as a raw ANSI string (truecolor escapes + powerline arrows), computes per-tab widths, and records click regions. No Zellij UI components — everything is hand-rendered ANSI written to stdout.
- `src/tab_pane_map.rs` — builds `pane_id → (tab_index, tab_name)` from `PaneManifest` + `TabInfo`, skipping plugin panes. This is how a hook event (which only knows its `ZELLIJ_PANE_ID`) gets associated with a tab.
- `src/theme.rs` — the `Theme` struct: every color the bar draws. At runtime it is derived from Zellij's own theme via `Theme::from_styling(&mode_info.style.colors)` (set on `ModeUpdate`), so the bar follows the user's Zellij theme and switches dark/light automatically (Zellij re-sends `ModeUpdate` on `toggle-theme`). `Theme::fallback()` is the only hardcoded palette, used until the first `ModeUpdate`. `render.rs` holds no color literals — it reads `state.theme` (copied out at the top of render to avoid borrow conflicts).
- `src/installer.rs` — generates and runs the self-install shell command (writes the hook script, registers hooks in `settings.json`).
- `scripts/zellaude-hook.sh` — the hook bridge. Embedded into the binary via `include_str!` and written to disk at runtime. Also owns the desktop-notification logic (rate-limited, focus-aware) so notifications fire once regardless of how many plugin instances exist.
- `scripts/install-hooks.sh` — standalone hook registration (used by `install.sh`); mirrors the logic in `installer.rs`.

### Key mechanisms

**Activity state machine.** Each Claude session (keyed by `pane_id`) has an `Activity`. Hook events map to activities in `event_handler.rs`: `PreToolUse → Tool(name)`, `PostToolUse/UserPromptSubmit → Thinking`, `PermissionRequest → Waiting`, `Stop/Notification → Prompting` (waiting for your input), `SubagentStop → AgentDone`, `SessionStart → Init`, `SessionEnd → remove`. The two "needs you" states rank highest in `activity_priority` (Waiting > Prompting > everything busy), so a tab with any pane awaiting you surfaces over merely-busy panes. `Prompting` does **not** decay — it persists until you act (your next prompt flips it to Thinking); only `AgentDone` decays to `Idle` after `DONE_TIMEOUT` (30s). When a tab has multiple sessions, render picks the highest-priority one.

**Attention flashing.** `should_flash` (event_handler) drives the tab flash: `PermissionRequest` (loud — the hook script also fires a macOS desktop notification) and `Notification` (quiet nudge — flash only, no desktop notification; this is the "you've left Claude waiting" case). A plain `Stop` shows `▶` without flashing. Flash respects the `flash` setting (Once/Persist/Off) and clears when you focus the tab or the session moves off a needs-you state. **macOS desktop notifications fire only for `PermissionRequest`** — and only from the hook script (one per pane), never the plugin, to avoid N-instance duplicates.

**Event ordering.** Async hooks race through parallel subprocesses, so each event carries `ts_ms` (captured in the hook script). `handle_hook_event` drops any event whose `ts_ms` is older than the session's `last_ts_ms`. A missing `ts_ms` (old hook script) is treated as fresh. This is the fix for stuck states — keep it in mind when touching event handling.

**Multi-instance sync.** Instances coordinate via named pipe messages handled in `pipe()`: `zellaude:request` (ask others for state), `zellaude:sync` (share sessions; merged by newest `last_event_ts` in `merge_sessions`), `zellaude:settings` (broadcast settings changes), `zellaude:focus` (notification-click → focus a pane). On `PermissionRequestResult` a new instance broadcasts a request so it catches up.

**Bootstrapping / permissions.** `load()` requests Zellij permissions and tries to load config immediately. Until `PermissionRequestResult` fires, `run_command` calls are dropped — so config load and hook install are retried there. `set_selectable(false)` is also deferred to that handler so the plugin stays visible in fullscreen. Don't move that call back into `load()` (regression history: permission dialog became unreachable).

**Config & settings.** Settings persist to `~/.config/zellij/plugins/zellaude.json`, read/written via `run_command` shelling out to `cat`/`printf` (WASM has no direct filesystem access to that path). `save_config` no-ops until `config_loaded` is true to avoid clobbering on startup. The hook script reads the same JSON directly for its notification mode.

**Rendering.** `render.rs` writes ANSI manually. Width accounting is explicit: every byte written must advance `col`, and the bar must never exceed `cols` (overflow clips, never scrolls — note the `\x1b[?7l` no-wrap escape). Colors are truecolor `\x1b[38;2;r;g;bm`, all sourced from `Theme` (see `theme.rs`) — never add color literals to `render.rs`. Click regions (`click_regions`, `menu_click_regions`, `prefix_click_region`) are rebuilt every render and consumed by the `Mouse::LeftClick` handler in `main.rs` — column ranges must stay in sync with what was drawn.

**Theming.** All colors follow the active Zellij theme by mapping Zellij's `Styling` semantic slots onto the bar: tab backgrounds ← `ribbon_selected/unselected`, bar background & accents ← `text_unselected` (`emphasis_0..3`), waiting/done ← `exit_code_error/success`. Zellij's `Styling` exposes ~6 distinct accent hues but no yellow; the one hue the bar needs and Zellij doesn't provide (yellow, for notifications) is **synthesized** via `with_hue()` — CSS-`hsl()`-style: take the theme's orange, keep its saturation/lightness, force the hue to yellow. So derived colors still adapt to dark/light. This requires Zellij ≥ 0.44 (where dark/light theme switching and the needed plugin API landed) — `zellij-tile` is pinned to 0.44.x for this reason.

## Versioning

`installer.rs` tags the installed hook script with `# zellaude v<CARGO_PKG_VERSION>` (from `Cargo.toml`). On load the plugin checks this tag and re-installs the hook + re-registers settings when the version changes. Bumping the version in `Cargo.toml` is what triggers users' hooks to update.
