use crate::state::{Activity, FlashMode, HookPayload, SessionInfo, State};

pub fn handle_hook_event(state: &mut State, payload: HookPayload) {
    // Capture env info for use in notifications
    if let Some(ref name) = payload.zellij_session {
        state.zellij_session_name = Some(name.clone());
    }
    if let Some(ref tp) = payload.term_program {
        state.term_program = Some(tp.clone());
    }

    let event = payload.hook_event.as_str();

    // SessionEnd → remove session (never drop: terminal cleanup)
    if event == "SessionEnd" {
        state.sessions.remove(&payload.pane_id);
        state.flash_deadlines.remove(&payload.pane_id);
        state.acked_panes.remove(&payload.pane_id);
        return;
    }

    // Drop events that arrive out of order (async hooks can race through
    // parallel subprocesses). Only enforced when the hook supplied ts_ms —
    // an absent field means an old hook script and is treated as fresh.
    if let Some(ts_ms) = payload.ts_ms {
        if let Some(session) = state.sessions.get(&payload.pane_id) {
            if ts_ms < session.last_ts_ms {
                return;
            }
        }
    }

    let activity = match event {
        "SessionStart" => Activity::Init,
        "PreToolUse" => {
            Activity::Tool(payload.tool_name.clone().unwrap_or_default())
        }
        "PostToolUse" | "PostToolUseFailure" => Activity::Thinking,
        "UserPromptSubmit" => Activity::Thinking,
        "PermissionRequest" => Activity::Waiting,
        // Stop = the turn ended → it's your turn to type.
        // Notification = Claude is actively waiting for your input (e.g. you've
        // been idle). Both mean "waiting for you", shown as Prompting (▶).
        "Stop" | "Notification" => Activity::Prompting,
        "SubagentStop" => Activity::AgentDone,
        _ => Activity::Idle,
    };

    // Claude resumed work → a new turn began, so forget any prior acknowledgement.
    // The next time this pane waits for you, it gets to flash again.
    let resumed = matches!(
        event,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "UserPromptSubmit" | "SessionStart"
    );
    if resumed {
        state.acked_panes.remove(&payload.pane_id);
    }

    // "Needs you" is muted once acknowledged: either you're on the pane right
    // now, or you already looked at it this waiting episode (acked_panes). A
    // repeat Notification is the same nudge for the same wait — stay quiet.
    // (Permission is blocking; it is never gated and always flags.)
    let on_focused_pane = state.focused_pane == Some(payload.pane_id);
    let acked = state.acked_panes.contains(&payload.pane_id);
    let was_prompting = matches!(activity, Activity::Prompting);
    let suppressed = was_prompting && (on_focused_pane || acked);
    let activity = if suppressed { Activity::Idle } else { activity };

    // Flash to draw attention when Claude needs you. Permission (Waiting) is the
    // loud case — the hook script also fires a desktop notification for it. A
    // Notification is the quieter "you've left me waiting" nudge: flash only,
    // never a desktop notification, and not while you're on or have acked the pane.
    let should_flash = matches!(event, "PermissionRequest")
        || (matches!(event, "Notification") && !on_focused_pane && !acked);

    let (tab_index, tab_name) = state
        .pane_to_tab
        .get(&payload.pane_id)
        .cloned()
        .unzip();

    let session = state
        .sessions
        .entry(payload.pane_id)
        .or_insert_with(|| SessionInfo {
            session_id: payload.session_id.clone().unwrap_or_default(),
            pane_id: payload.pane_id,
            activity: Activity::Init,
            tab_name: None,
            tab_index: None,
            last_event_ts: 0,
            cwd: None,
            last_ts_ms: 0,
        });

    if should_flash {
        match state.settings.flash {
            FlashMode::Once => {
                state.flash_deadlines.insert(
                    payload.pane_id,
                    crate::state::unix_now_ms() + crate::state::FLASH_DURATION_MS,
                );
            }
            FlashMode::Persist => {
                state.flash_deadlines.insert(payload.pane_id, u64::MAX);
            }
            FlashMode::Off => {}
        }
        // Desktop notification (permission only) is handled by the hook script
        // to avoid duplicates from multiple plugin instances.
    } else {
        state.flash_deadlines.remove(&payload.pane_id);
    }

    session.activity = activity;
    session.last_event_ts = crate::state::unix_now();
    if let Some(ts_ms) = payload.ts_ms {
        session.last_ts_ms = ts_ms;
    }
    if let Some(sid) = &payload.session_id {
        session.session_id = sid.clone();
    }
    if let Some(cwd) = payload.cwd {
        session.cwd = Some(cwd);
    }
    if let Some((idx, name)) = tab_index.zip(tab_name) {
        session.tab_index = Some(idx);
        session.tab_name = Some(name);
    }

    // You're on this pane as it started waiting — a fresh acknowledgement.
    // Remember it (so later background Notifications stay quiet) and tell the
    // other instances, which got the same hook but lack accurate focus and may
    // have set Prompting, to clear it so the bar agrees across tabs.
    if was_prompting && on_focused_pane && !acked {
        state.acked_panes.insert(payload.pane_id);
        state.broadcast_ack(payload.pane_id);
    }
}
