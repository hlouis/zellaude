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

    // If you're already looking at this pane, a "waiting for your input" needn't
    // flag it — you're here. (Permission still flags; it is blocking.)
    let on_focused_pane = state.focused_pane == Some(payload.pane_id);
    let activity = if on_focused_pane && matches!(activity, Activity::Prompting) {
        Activity::Idle
    } else {
        activity
    };

    // Flash to draw attention when Claude needs you. Permission (Waiting) is the
    // loud case — the hook script also fires a desktop notification for it. A
    // Notification is the quieter "you've left me waiting" nudge: flash only,
    // never a desktop notification, and not while you're on the pane.
    let should_flash = matches!(event, "PermissionRequest")
        || (matches!(event, "Notification") && !on_focused_pane);

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
}
