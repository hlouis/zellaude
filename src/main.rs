mod event_handler;
mod installer;
mod render;
mod state;
mod tab_pane_map;
mod theme;

use state::{unix_now, unix_now_ms, HookPayload, MenuAction, SessionInfo, Settings, State, ViewMode};
use std::collections::BTreeMap;
use zellij_tile::prelude::*;

const DONE_TIMEOUT: u64 = 30;
const TIMER_INTERVAL: f64 = 1.0;
const FLASH_TICK: f64 = 0.25;

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
            PermissionType::ReadCliPipes,
            PermissionType::MessageAndLaunchOtherPlugins,
        ]);
        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::ModeUpdate,
            EventType::Timer,
            EventType::Mouse,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
        set_timeout(TIMER_INTERVAL);

        // Load persisted settings (may be retried in PermissionRequestResult
        // if this fires before permissions are granted)
        self.load_config();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::TabUpdate(tabs) => {
                let new_active = tabs.iter().find(|t| t.active).map(|t| t.position);
                if new_active != self.active_tab_index {
                    // Tab focus changed — clear persist flashes on the newly focused tab
                    if let Some(idx) = new_active {
                        self.clear_flashes_on_tab(idx);
                    }
                }
                self.active_tab_index = new_active;
                self.tabs = tabs;
                self.rebuild_pane_map();
                self.refresh_focus();
                self.publish_focus();
                true
            }
            Event::PaneUpdate(manifest) => {
                self.pane_manifest = Some(manifest);
                self.rebuild_pane_map();
                self.refresh_focus();
                self.publish_focus();
                true
            }
            Event::ModeUpdate(mode_info) => {
                self.input_mode = mode_info.mode;
                if let Some(name) = mode_info.session_name {
                    self.zellij_session_name = Some(name);
                }
                // Follow the active Zellij theme (re-fires on dark/light toggle)
                self.theme = theme::Theme::from_styling(&mode_info.style.colors);
                true
            }
            Event::Mouse(Mouse::LeftClick(_, col)) => {
                let col = col as usize;

                // Check prefix click region first → toggle ViewMode
                if let Some((start, end)) = self.prefix_click_region {
                    if col >= start && col < end {
                        self.view_mode = match self.view_mode {
                            ViewMode::Normal => ViewMode::Settings,
                            ViewMode::Settings => ViewMode::Normal,
                        };
                        return true;
                    }
                }

                match self.view_mode {
                    ViewMode::Normal => {
                        for region in &self.click_regions {
                            if col >= region.start_col && col < region.end_col {
                                if region.is_waiting {
                                    focus_terminal_pane(region.pane_id, false, false);
                                } else {
                                    switch_tab_to(region.tab_index as u32 + 1);
                                }
                                return false;
                            }
                        }
                        false
                    }
                    ViewMode::Settings => {
                        for region in &self.menu_click_regions {
                            if col >= region.start_col && col < region.end_col {
                                match &region.action {
                                    MenuAction::ToggleSetting(key) => {
                                        match key {
                                            state::SettingKey::Notifications => {
                                                self.settings.notifications =
                                                    self.settings.notifications.cycle();
                                            }
                                            state::SettingKey::Flash => {
                                                self.settings.flash =
                                                    self.settings.flash.cycle();
                                            }
                                            state::SettingKey::ElapsedTime => {
                                                self.settings.elapsed_time =
                                                    !self.settings.elapsed_time;
                                            }
                                            state::SettingKey::ModeIndicator => {
                                                self.settings.mode_indicator =
                                                    !self.settings.mode_indicator;
                                            }
                                        }
                                        self.save_config();
                                    }
                                    MenuAction::CloseMenu => {
                                        self.view_mode = ViewMode::Normal;
                                    }
                                }
                                return true;
                            }
                        }
                        false
                    }
                }
            }
            Event::RunCommandResult(exit_code, stdout, _stderr, context) => {
                match context.get("type").map(|s| s.as_str()) {
                    Some("load_config") if exit_code == Some(0) => {
                        let raw = String::from_utf8_lossy(&stdout);
                        if let Ok(settings) = serde_json::from_str::<Settings>(raw.trim()) {
                            self.settings = settings;
                        }
                        self.config_loaded = true;
                        true
                    }
                    Some("install_hooks") => {
                        self.hooks_installed = true;
                        false
                    }
                    _ => false,
                }
            }
            Event::Timer(_) => {
                let stale_changed = self.cleanup_stale_sessions();
                let flash_changed = self.cleanup_expired_flashes();
                let has_flashes = self.has_active_flashes();
                if has_flashes {
                    set_timeout(FLASH_TICK);
                } else {
                    set_timeout(TIMER_INTERVAL);
                }
                has_flashes || stale_changed || flash_changed || self.has_elapsed_display()
            }
            Event::PermissionRequestResult(_) => {
                // Now that permissions are granted, mark as non-selectable
                // so the plugin stays visible during fullscreen
                set_selectable(false);
                // Permissions granted — ask existing instances for their state
                self.request_sync();
                // Retry config load (the one in load() may have been dropped
                // because it ran before permissions were granted)
                if !self.config_loaded {
                    self.load_config();
                }
                // Auto-install hook script and register Claude Code hooks
                if !self.hooks_installed {
                    installer::run_install();
                }
                // Any publish before this point was dropped along with every
                // other run_command — clear the debounce and write it now.
                self.published_focus = None;
                self.publish_focus();
                false
            }
            _ => false,
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        match pipe_message.name.as_str() {
            "zellaude" => {
                // Hook event from CLI
                let payload_str = match pipe_message.payload {
                    Some(ref s) => s,
                    None => return false,
                };
                let payload: HookPayload = match serde_json::from_str(payload_str) {
                    Ok(p) => p,
                    Err(_) => return false,
                };
                event_handler::handle_hook_event(self, payload);
                true
            }
            "zellaude:focus" => {
                // Notification click — focus the requested pane
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(pane_id) = payload.trim().parse::<u32>() {
                        focus_terminal_pane(pane_id, false, false);
                    }
                }
                false
            }
            "zellaude:request" => {
                // Another instance asking for state — respond with ours
                self.broadcast_sessions();
                false
            }
            "zellaude:ack" => {
                // Another instance reports the user is looking at a pane —
                // clear its ▶/flash here too so every instance agrees.
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(pane_id) = payload.trim().parse::<u32>() {
                        return self.apply_ack(pane_id);
                    }
                }
                false
            }
            "zellaude:settings" => {
                // Another instance broadcast new settings
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(settings) = serde_json::from_str::<Settings>(payload) {
                        self.settings = settings;
                        return true;
                    }
                }
                false
            }
            "zellaude:dump" => {
                // Debug: each instance writes its own state to stderr, which
                // Zellij captures into its log (auto-tagged with the plugin id).
                // We can't write to the invoking terminal's stdout: a CLI pipe
                // broadcasts to EVERY instance, and N writers on one pipe make
                // cli_pipe_output race and loop ("1000 unknown messages, logging
                // client out"). stderr→log is the robust path for N instances.
                self.dump_state();
                // CLI pipes block by default — unblock so `zellij pipe` returns
                // immediately instead of hanging to a 1s timeout.
                if let PipeSource::Cli(ref input_pipe_id) = pipe_message.source {
                    unblock_cli_pipe_input(input_pipe_id);
                }
                false
            }
            "zellaude:sync" => {
                // Another instance sharing state — merge it
                if let Some(ref payload) = pipe_message.payload {
                    if let Ok(sessions) =
                        serde_json::from_str::<BTreeMap<u32, SessionInfo>>(payload)
                    {
                        self.merge_sessions(sessions);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        render::render_status_bar(self, rows, cols);
    }
}

impl State {
    fn rebuild_pane_map(&mut self) {
        if let Some(ref manifest) = self.pane_manifest {
            let (pane_to_tab, pane_pos) =
                tab_pane_map::build_pane_to_tab_map(&self.tabs, manifest);
            self.pane_to_tab = pane_to_tab;
            self.pane_pos = pane_pos;
            self.refresh_session_tab_names();
            self.remove_dead_panes();
        }
    }

    /// The focused terminal pane in the active tab, per the pane manifest.
    fn focused_terminal_pane(&self) -> Option<u32> {
        let manifest = self.pane_manifest.as_ref()?;
        let active = self.active_tab_index?;
        manifest
            .panes
            .get(&active)?
            .iter()
            .find(|p| p.is_focused && !p.is_plugin)
            .map(|p| p.id)
    }

    /// Track which pane is focused, and reset the "waiting for your input"
    /// flag on it: once you're looking at a pane, its ▶ has done its job.
    /// Leaves Waiting (permission) alone — focusing isn't answering.
    fn refresh_focus(&mut self) {
        let focused = self.focused_terminal_pane();
        self.focused_pane = focused;
        if let Some(pane_id) = focused {
            let is_prompting = self
                .sessions
                .get(&pane_id)
                .is_some_and(|s| matches!(s.activity, state::Activity::Prompting));
            if is_prompting {
                if let Some(s) = self.sessions.get_mut(&pane_id) {
                    s.activity = state::Activity::Idle;
                }
                self.flash_deadlines.remove(&pane_id);
                // Remember you looked, so repeat Notifications for this waiting
                // episode stay quiet until Claude does real work again.
                self.acked_panes.insert(pane_id);
                // Tell the other instances — they render this tab when active
                // and won't have seen the focus locally.
                self.broadcast_ack(pane_id);
            }
        }
    }

    /// Broadcast "the user is looking at this pane" so every instance clears
    /// its ▶/flash for it. The focus reset is otherwise local to whichever
    /// instance owns the active tab, so a different instance rendering the bar
    /// would still show the stale ▶.
    fn broadcast_ack(&self, pane_id: u32) {
        let mut msg = MessageToPlugin::new("zellaude:ack");
        msg.message_payload = Some(pane_id.to_string());
        pipe_message_to_plugin(msg);
    }

    /// Clear a pane's ▶/flash because the user looked at it. Idempotent.
    fn apply_ack(&mut self, pane_id: u32) -> bool {
        // Mirror the focusing instance's acknowledgement locally so repeat
        // Notifications stay quiet in this instance too.
        self.acked_panes.insert(pane_id);
        let mut changed = false;
        if let Some(s) = self.sessions.get_mut(&pane_id) {
            if matches!(s.activity, state::Activity::Prompting) {
                s.activity = state::Activity::Idle;
                changed = true;
            }
        }
        if self.flash_deadlines.remove(&pane_id).is_some() {
            changed = true;
        }
        changed
    }

    fn refresh_session_tab_names(&mut self) {
        for session in self.sessions.values_mut() {
            if let Some((idx, name)) = self.pane_to_tab.get(&session.pane_id) {
                session.tab_index = Some(*idx);
                session.tab_name = Some(name.clone());
            }
        }
    }

    fn remove_dead_panes(&mut self) {
        self.sessions
            .retain(|pane_id, _| self.pane_to_tab.contains_key(pane_id));
        let live = &self.pane_to_tab;
        self.acked_panes.retain(|pane_id| live.contains_key(pane_id));
    }

    fn cleanup_stale_sessions(&mut self) -> bool {
        let now = unix_now();
        let mut changed = false;
        for session in self.sessions.values_mut() {
            // A finished subagent decays to Idle. Prompting (waiting for your
            // input) is intentionally NOT decayed — it stays until you act.
            if matches!(session.activity, state::Activity::AgentDone)
                && now.saturating_sub(session.last_event_ts) >= DONE_TIMEOUT
            {
                session.activity = state::Activity::Idle;
                changed = true;
            }
        }
        changed
    }

    fn clear_flashes_on_tab(&mut self, tab_idx: usize) {
        let pane_ids: Vec<u32> = self
            .sessions
            .values()
            .filter(|s| s.tab_index == Some(tab_idx))
            .map(|s| s.pane_id)
            .collect();
        for pane_id in pane_ids {
            self.flash_deadlines.remove(&pane_id);
        }
    }

    fn has_active_flashes(&self) -> bool {
        let now = unix_now_ms();
        self.flash_deadlines.values().any(|&deadline| now < deadline)
    }

    fn cleanup_expired_flashes(&mut self) -> bool {
        let before = self.flash_deadlines.len();
        let now = unix_now_ms();
        self.flash_deadlines.retain(|_, deadline| now < *deadline);
        self.flash_deadlines.len() != before
    }

    fn has_elapsed_display(&self) -> bool {
        if !self.settings.elapsed_time {
            return false;
        }
        let now = unix_now();
        self.sessions.values().any(|s| {
            !matches!(s.activity, state::Activity::Idle)
                && now.saturating_sub(s.last_event_ts) >= DONE_TIMEOUT
        })
    }

    fn request_sync(&self) {
        pipe_message_to_plugin(MessageToPlugin::new("zellaude:request"));
    }

    fn broadcast_sessions(&self) {
        let mut msg = MessageToPlugin::new("zellaude:sync");
        msg.message_payload =
            Some(serde_json::to_string(&self.sessions).unwrap_or_default());
        pipe_message_to_plugin(msg);
    }

    fn broadcast_settings(&self) {
        let mut msg = MessageToPlugin::new("zellaude:settings");
        msg.message_payload =
            Some(serde_json::to_string(&self.settings).unwrap_or_default());
        pipe_message_to_plugin(msg);
    }

    /// Debug snapshot: write this instance's internal state to stderr as one
    /// compact JSON line (Zellij captures it into its log, tagged `[id: N]`).
    /// One line per instance — that exposes any per-instance divergence in
    /// focused_pane / acked_panes / flash_deadlines. Trigger with
    /// `zellij pipe --name zellaude:dump`, then read it back from the log:
    ///   grep zellaude-dump <log> | sed 's/.*zellaude-dump //' | jq -s .
    fn dump_state(&self) {
        let dump = serde_json::json!({
            "plugin_id": get_plugin_ids().plugin_id,
            "now_ms": unix_now_ms(),
            "active_tab_index": self.active_tab_index,
            "focused_pane": self.focused_pane,
            "config_loaded": self.config_loaded,
            "hooks_installed": self.hooks_installed,
            "settings": self.settings,
            "sessions": self.sessions,
            // raw deadline; compare against now_ms (u64::MAX == FlashMode::Persist)
            "flash_deadlines": self.flash_deadlines,
            "acked_panes": self.acked_panes,
            "pane_to_tab": self.pane_to_tab,
            // pane_id -> [x, y]; icon draw order sorts by this.
            "pane_pos": self.pane_pos,
            // Colors as derived from Zellij's Styling — the only way to see what
            // the bar is actually painting when a theme reads badly.
            "theme": self.theme.dump(),
        });
        // compact (single line) so each instance is one greppable log entry
        let json = serde_json::to_string(&dump).unwrap_or_default();
        eprintln!("zellaude-dump {json}");
    }

    /// Publish the *on-screen* Zellij tab's terminal pane ids to a per-session
    /// file, so the hook script can tell "you are looking at this pane's tab"
    /// from "this pane is buried in a background tab". The hook only knows its
    /// own `$ZELLIJ_PANE_ID`; the pane → tab mapping lives here.
    ///
    /// No leader election: every instance computes the same list and writes it.
    /// The write is temp-file + `mv` (atomic rename), so racing writers can
    /// only ever replace the file with identical bytes and a reader never sees
    /// a half-written file. `published_focus` skips the redundant re-writes.
    fn publish_focus(&mut self) {
        let (Some(session), Some(active)) = (
            self.zellij_session_name.as_deref(),
            self.active_tab_index,
        ) else {
            return;
        };
        let mut panes: Vec<u32> = self
            .pane_to_tab
            .iter()
            .filter(|(_, (tab_index, _))| *tab_index == active)
            .map(|(pane_id, _)| *pane_id)
            .collect();
        // Sorted so identical state produces identical bytes — otherwise the
        // HashMap iteration order alone would defeat the debounce below.
        panes.sort_unstable();
        let json = serde_json::json!({ "active_panes": panes }).to_string();
        if self.published_focus.as_deref() == Some(json.as_str()) {
            return;
        }
        self.published_focus = Some(json.clone());

        // Same sanitizing as the hook script's lock files: pane ids are only
        // unique within a session, so the file must be session-qualified.
        let safe: String = session
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let json_esc = json.replace('\'', "'\\''");
        let cmd = format!(
            "f=/tmp/zellaude-focus-{safe}.json; printf '%s' '{json_esc}' > \"$f.$$\" && mv -f \"$f.$$\" \"$f\""
        );
        let mut ctx = BTreeMap::new();
        ctx.insert("type".into(), "publish_focus".into());
        run_command(&["sh", "-c", &cmd], ctx);
    }

    fn load_config(&self) {
        let mut ctx = BTreeMap::new();
        ctx.insert("type".into(), "load_config".into());
        run_command(
            &[
                "sh",
                "-c",
                "cat \"$HOME/.config/zellij/plugins/zellaude.json\" 2>/dev/null || echo '{}'",
            ],
            ctx,
        );
    }

    fn save_config(&self) {
        if !self.config_loaded {
            return;
        }
        self.broadcast_settings();
        let json = serde_json::to_string(&self.settings).unwrap_or_default();
        let json_esc = json.replace('\'', "'\\''");
        let cmd = format!(
            "mkdir -p \"$HOME/.config/zellij/plugins\" && printf '%s' '{json_esc}' > \"$HOME/.config/zellij/plugins/zellaude.json\""
        );
        let mut ctx = BTreeMap::new();
        ctx.insert("type".into(), "save_config".into());
        run_command(&["sh", "-c", &cmd], ctx);
    }

    fn merge_sessions(&mut self, incoming: BTreeMap<u32, SessionInfo>) {
        for (pane_id, mut session) in incoming {
            let dominated = self
                .sessions
                .get(&pane_id)
                .map(|existing| session.last_event_ts > existing.last_event_ts)
                .unwrap_or(true);
            if dominated {
                // Refresh tab name from our local pane map
                if let Some((idx, name)) = self.pane_to_tab.get(&pane_id) {
                    session.tab_index = Some(*idx);
                    session.tab_name = Some(name.clone());
                }
                self.sessions.insert(pane_id, session);
            }
        }
    }
}
