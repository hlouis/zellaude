use crate::state::{
    unix_now, unix_now_ms, Activity, ClickRegion, FlashMode, MenuAction, MenuClickRegion,
    NotifyMode, SessionInfo, SettingKey, State, ViewMode,
};
use crate::theme::{Rgb, Theme};
use std::fmt::Write;
use std::io::Write as IoWrite;
use zellij_tile::prelude::{InputMode, TabInfo};

struct Style {
    symbol: &'static str,
    color: Rgb,
}

fn activity_priority(activity: &Activity) -> u8 {
    // "Needs you" states rank highest so a tab with any pane awaiting you
    // surfaces over panes that are merely busy.
    match activity {
        Activity::Waiting => 6,   // needs permission/answer
        Activity::Prompting => 5, // your turn to type
        Activity::Tool(_) => 4,
        Activity::Thinking => 3,
        Activity::Init => 2,
        Activity::AgentDone => 1,
        Activity::Idle => 0,
    }
}

/// Symbol per activity is fixed; color comes from the theme's accent palette.
fn activity_style(activity: &Activity, theme: &Theme) -> Style {
    match activity {
        Activity::Init => Style { symbol: "◆", color: theme.gray },
        Activity::Thinking => Style { symbol: "●", color: theme.purple },
        Activity::Tool(name) => {
            let symbol = match name.as_str() {
                "Bash" => "⚡",
                "Read" | "Glob" | "Grep" => "◉",
                "Edit" | "Write" => "✎",
                "Task" => "⊜",
                "WebSearch" | "WebFetch" => "◈",
                _ => "⚙",
            };
            Style { symbol, color: theme.orange }
        }
        Activity::Prompting => Style { symbol: "▶", color: theme.green },
        Activity::Waiting => Style { symbol: "⚠", color: theme.red },
        Activity::AgentDone => Style { symbol: "✓", color: theme.green },
        Activity::Idle => Style { symbol: "○", color: theme.gray },
    }
}

fn fg((r, g, b): Rgb) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn bg((r, g, b): Rgb) -> String {
    format!("\x1b[48;2;{r};{g};{b}m")
}

fn display_width(s: &str) -> usize {
    s.chars().count()
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ELAPSED_THRESHOLD: u64 = 30;
const SEPARATOR: &str = "\u{e0b0}";

/// Write a powerline arrow: fg=from_bg, bg=to_bg, then separator char.
fn arrow(buf: &mut String, col: &mut usize, from: Rgb, to: Rgb) {
    let _ = write!(buf, "{}{}{SEPARATOR}", fg(from), bg(to));
    *col += 1;
}

fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Background color + label per input mode. Color is drawn from the theme's
/// accent palette so the mode pill follows the active Zellij theme.
fn mode_style(mode: InputMode, theme: &Theme) -> (Rgb, &'static str) {
    match mode {
        InputMode::Normal => (theme.green, "NORMAL"),
        InputMode::Locked => (theme.red, "LOCKED"),
        InputMode::Pane => (theme.cyan, "PANE"),
        InputMode::Tab => (theme.purple, "TAB"),
        InputMode::Resize => (theme.orange, "RESIZE"),
        InputMode::Move => (theme.orange, "MOVE"),
        InputMode::Scroll => (theme.yellow, "SCROLL"),
        InputMode::EnterSearch => (theme.yellow, "SEARCH"),
        InputMode::Search => (theme.yellow, "SEARCH"),
        InputMode::RenameTab => (theme.yellow, "RENAME"),
        InputMode::RenamePane => (theme.yellow, "RENAME"),
        InputMode::Session => (theme.purple, "SESSION"),
        InputMode::Prompt => (theme.green, "PROMPT"),
        InputMode::Tmux => (theme.green, "TMUX"),
    }
}

pub fn render_status_bar(state: &mut State, _rows: usize, cols: usize) {
    state.click_regions.clear();
    state.menu_click_regions.clear();

    // Copy the theme out so we can borrow `state` mutably below.
    let theme = state.theme;

    let mut buf = String::with_capacity(cols * 4);
    // Terminal setup for a 1-row status bar:
    //  \x1b[H     — cursor home (prevent scroll from cursor at end-of-line)
    //  \x1b[?7l   — disable auto-wrap (clip overflow instead of scroll)
    //  \x1b[?25l  — hide cursor
    buf.push_str("\x1b[H\x1b[?7l\x1b[?25l");
    let bar_bg_str = bg(theme.bar_bg);

    // Bail early if terminal is too narrow
    if cols < 5 {
        let _ = write!(buf, "{bar_bg_str}{:width$}{RESET}", "", width = cols);
        print!("{buf}");
        let _ = std::io::stdout().flush();
        return;
    }

    let prefix_bg = if state.view_mode == ViewMode::Settings {
        theme.prefix_bg_settings
    } else {
        theme.prefix_bg
    };

    // Build prefix: " Zellaude (session) MODE "
    let (mode_bg, mode_text) = mode_style(state.input_mode, &theme);
    let show_mode = state.settings.mode_indicator;
    let session_part = match state.zellij_session_name.as_deref() {
        Some(name) => format!(" ({name})"),
        None => String::new(),
    };
    let prefix_text = format!(" Zellaude{session_part} ");
    let prefix_width = display_width(&prefix_text);
    let mode_pill_width = if show_mode { 1 + mode_text.len() + 1 } else { 0 };
    let total_prefix_width = prefix_width + mode_pill_width;

    // Foreground matched to the prefix background's declaration, so it stays
    // readable: settings → ribbon_selected pair, normal → ribbon_unselected.
    let prefix_fg = if state.view_mode == ViewMode::Settings {
        theme.text_active
    } else {
        theme.text_inactive
    };

    // Render prefix segment (truncate if wider than cols)
    let mut col;
    if total_prefix_width <= cols {
        let _ = write!(
            buf,
            "{}{}{BOLD}{prefix_text}{RESET}",
            bg(prefix_bg),
            fg(prefix_fg),
        );
        if show_mode {
            let _ = write!(
                buf,
                "{}{}{BOLD} {mode_text} {RESET}",
                bg(mode_bg),
                fg(theme.on(mode_bg)),
            );
        }
        col = total_prefix_width;
    } else if prefix_width <= cols {
        // Fit the name part but skip mode pill
        let _ = write!(
            buf,
            "{}{}{BOLD}{prefix_text}{RESET}",
            bg(prefix_bg),
            fg(prefix_fg),
        );
        col = prefix_width;
    } else {
        // Even name doesn't fit — just show what we can
        let avail = cols.saturating_sub(2); // leave room for fill
        let short: String = prefix_text.chars().take(avail).collect();
        let _ = write!(
            buf,
            "{}{}{BOLD}{short}{RESET}",
            bg(prefix_bg),
            fg(prefix_fg),
        );
        col = display_width(&short);
    }
    state.prefix_click_region = Some((0, col));

    let last_prefix_bg = if show_mode && total_prefix_width <= cols { mode_bg } else { prefix_bg };
    let prefix_used = col;

    if col < cols {
        match state.view_mode {
            ViewMode::Normal => {
                render_tabs(state, &theme, &mut buf, &mut col, cols, last_prefix_bg, prefix_used);
            }
            ViewMode::Settings => {
                arrow(&mut buf, &mut col, last_prefix_bg, theme.bar_bg);
                let _ = write!(buf, "{bar_bg_str}");
                render_settings_menu(state, &theme, &mut buf, &mut col);
            }
        }
    }

    // Fill remaining width with bar background — never exceed cols
    if col < cols {
        let remaining = cols - col;
        let _ = write!(buf, "{bar_bg_str}{:width$}", "", width = remaining);
    }
    let _ = write!(buf, "{RESET}");

    print!("{buf}");
    let _ = std::io::stdout().flush();
}

fn render_tabs(
    state: &mut State,
    theme: &Theme,
    buf: &mut String,
    col: &mut usize,
    cols: usize,
    prefix_bg: Rgb,
    prefix_width: usize,
) {
    let now_s = unix_now();
    let now_ms = unix_now_ms();

    // Sort tabs by position
    let mut tabs: Vec<&TabInfo> = state.tabs.iter().collect();
    tabs.sort_by_key(|t| t.position);

    let count = tabs.len();
    if count == 0 {
        arrow(buf, col, prefix_bg, theme.bar_bg);
        return;
    }

    // For each tab, find the best (highest-priority) Claude session
    let best_sessions: Vec<Option<&SessionInfo>> = tabs
        .iter()
        .map(|tab| {
            state
                .sessions
                .values()
                .filter(|s| s.tab_index == Some(tab.position))
                .max_by_key(|s| activity_priority(&s.activity))
        })
        .collect();

    // Pre-compute elapsed strings (only for Claude tabs)
    let elapsed_strs: Vec<Option<String>> = best_sessions
        .iter()
        .map(|session: &Option<&SessionInfo>| {
            if !state.settings.elapsed_time {
                return None;
            }
            session.and_then(|s| {
                let elapsed = now_s.saturating_sub(s.last_event_ts);
                if elapsed >= ELAPSED_THRESHOLD {
                    Some(format_elapsed(elapsed))
                } else {
                    None
                }
            })
        })
        .collect();

    // Compute overhead: varies per tab type
    let total_elapsed_width: usize = elapsed_strs
        .iter()
        .map(|e: &Option<String>| e.as_ref().map_or(0, |s| s.len() + 1))
        .sum();
    let per_tab_overhead: usize = best_sessions
        .iter()
        .map(|s: &Option<&SessionInfo>| if s.is_some() { 4 } else { 2 })
        .sum();
    let overhead = prefix_width + 2 * count + per_tab_overhead + total_elapsed_width;
    let max_name_len = if overhead < cols {
        ((cols - overhead) / count).min(20)
    } else {
        0
    };

    let mut prev_bg = prefix_bg;

    for (i, tab) in tabs.iter().enumerate() {
        // Stop if we'd overflow — need room for at least arrow + closing arrow
        let arrows_needed = if prev_bg == prefix_bg { 1 } else { 2 };
        if *col + arrows_needed + 3 > cols {
            break;
        }

        let session = best_sessions[i];
        let is_claude = session.is_some();
        let tab_name = &tab.name;

        // Truncate name
        let char_count = tab_name.chars().count();
        let truncated = if max_name_len == 0 {
            String::new()
        } else if char_count > max_name_len {
            let s: String = tab_name.chars().take(max_name_len.saturating_sub(1)).collect();
            format!("{s}…")
        } else {
            tab_name.to_string()
        };

        // Check flash for any session in this tab
        let is_flash_bright = state
            .sessions
            .values()
            .filter(|s| s.tab_index == Some(tab.position))
            .any(|s| {
                state
                    .flash_deadlines
                    .get(&s.pane_id)
                    .map(|&deadline| now_ms < deadline && (now_ms / 250) % 2 == 0)
                    .unwrap_or(false)
            });

        let is_active = tab.active;

        // Pick tab background color
        let tab_bg = if is_flash_bright {
            theme.flash_bg
        } else if is_active {
            theme.tab_active_bg
        } else {
            theme.tab_inactive_bg
        };

        // Arrow: close previous segment, then open this tab
        if prev_bg == prefix_bg {
            arrow(buf, col, prev_bg, tab_bg);
        } else {
            arrow(buf, col, prev_bg, theme.bar_bg);
            arrow(buf, col, theme.bar_bg, tab_bg);
        }

        let tab_bg_str = bg(tab_bg);
        let region_start = *col;

        if is_claude {
            let s = session.unwrap();
            let style = activity_style(&s.activity, theme);

            let (sym_fg, name_fg, name_bold) = if is_flash_bright {
                (fg(theme.flash_text), fg(theme.flash_text), true)
            } else if is_active {
                (fg(style.color), fg(theme.text_active), true)
            } else {
                (fg(style.color), fg(theme.text_inactive), false)
            };

            // Leading space
            let _ = write!(buf, "{tab_bg_str} ");
            *col += 1;

            // Symbol
            let _ = write!(buf, "{sym_fg}{}", style.symbol);
            *col += display_width(style.symbol);

            // Space + name
            if !truncated.is_empty() {
                let bold_str = if name_bold { BOLD } else { "" };
                let _ = write!(buf, " {bold_str}{name_fg}{truncated}{RESET}{tab_bg_str}");
                *col += 1 + display_width(&truncated);
            }

            // Elapsed suffix — on the tab background, so use the tab's matched
            // base for contrast (text_dim pairs with the bar background).
            if let Some(ref es) = elapsed_strs[i] {
                if *col + 1 + es.len() + 1 < cols {
                    let elapsed_fg = if is_active { theme.text_active } else { theme.text_inactive };
                    let _ = write!(buf, " {}{es}", fg(elapsed_fg));
                    *col += 1 + es.len();
                }
            }

            // Fullscreen indicator
            if tab.is_fullscreen_active && *col + 3 < cols {
                let _ = write!(buf, " {}F{RESET}{tab_bg_str}", fg(theme.yellow));
                *col += 2;
            }

            // Trailing space
            let _ = write!(buf, " ");
            *col += 1;

            // Click region: if any session is waiting, use its pane_id for focus
            let waiting_session = state
                .sessions
                .values()
                .filter(|s| s.tab_index == Some(tab.position))
                .find(|s| matches!(s.activity, Activity::Waiting));

            state.click_regions.push(ClickRegion {
                start_col: region_start,
                end_col: *col,
                tab_index: tab.position,
                pane_id: waiting_session.map_or(0, |s| s.pane_id),
                is_waiting: waiting_session.is_some(),
            });
        } else {
            // Non-Claude tab: dimmer, no symbol
            let name_fg = if is_active {
                fg(theme.text_active)
            } else {
                fg(theme.text_inactive)
            };
            let name_bold = is_active;

            // Leading space
            let _ = write!(buf, "{tab_bg_str} ");
            *col += 1;

            // Name only (no symbol)
            if !truncated.is_empty() {
                let bold_str = if name_bold { BOLD } else { "" };
                let _ = write!(buf, "{bold_str}{name_fg}{truncated}{RESET}{tab_bg_str}");
                *col += display_width(&truncated);
            }

            // Fullscreen indicator
            if tab.is_fullscreen_active && *col + 3 < cols {
                let _ = write!(buf, " {}F{RESET}{tab_bg_str}", fg(theme.yellow));
                *col += 2;
            }

            // Trailing space
            let _ = write!(buf, " ");
            *col += 1;

            state.click_regions.push(ClickRegion {
                start_col: region_start,
                end_col: *col,
                tab_index: tab.position,
                pane_id: 0,
                is_waiting: false,
            });
        }

        prev_bg = tab_bg;
    }

    // Arrow from last tab → bar background (only if we rendered any tabs)
    if prev_bg != prefix_bg || count > 0 {
        arrow(buf, col, prev_bg, theme.bar_bg);
    }
}

/// Color for a setting's three states: on / partial / off.
fn tristate_colors(theme: &Theme, level: u8) -> (&'static str, Rgb, Rgb) {
    // Symbol color carries the state; label is always the bar's matched text
    // color so it stays readable on the bar background.
    match level {
        2 => ("●", theme.green, theme.text_dim),
        1 => ("◐", theme.yellow, theme.text_dim),
        _ => ("○", theme.gray, theme.text_dim),
    }
}

fn notify_mode_label(mode: NotifyMode, theme: &Theme) -> (&'static str, &'static str, Rgb, Rgb) {
    let (level, label) = match mode {
        NotifyMode::Always => (2u8, "Notify: always"),
        NotifyMode::Unfocused => (1, "Notify: unfocused"),
        NotifyMode::Never => (0, "Notify: off"),
    };
    let (symbol, sym_color, label_color) = tristate_colors(theme, level);
    (symbol, label, sym_color, label_color)
}

fn flash_mode_label(mode: FlashMode, theme: &Theme) -> (&'static str, &'static str, Rgb, Rgb) {
    let (level, label) = match mode {
        FlashMode::Persist => (2u8, "Flash: persist"),
        FlashMode::Once => (1, "Flash: brief"),
        FlashMode::Off => (0, "Flash: off"),
    };
    let (symbol, sym_color, label_color) = tristate_colors(theme, level);
    (symbol, label, sym_color, label_color)
}

/// Render a toggle and register its click region.
fn render_tristate(
    buf: &mut String,
    col: &mut usize,
    state_regions: &mut Vec<MenuClickRegion>,
    key: SettingKey,
    symbol: &str,
    label: &str,
    sym_color: Rgb,
    label_color: Rgb,
) {
    let region_start = *col;
    let width = display_width(symbol) + 1 + label.len();
    *col += width;

    state_regions.push(MenuClickRegion {
        start_col: region_start,
        end_col: *col,
        action: MenuAction::ToggleSetting(key),
    });

    let _ = write!(buf, "{}{symbol} {}{label}", fg(sym_color), fg(label_color));
}

fn render_settings_menu(state: &mut State, theme: &Theme, buf: &mut String, col: &mut usize) {
    // Leading space after arrow
    let _ = write!(buf, " ");
    *col += 1;

    // --- Notifications (three-state) ---
    {
        let (symbol, label, sym_color, label_color) =
            notify_mode_label(state.settings.notifications, theme);
        render_tristate(
            buf, col, &mut state.menu_click_regions,
            SettingKey::Notifications, symbol, label, sym_color, label_color,
        );
    }

    // --- Flash (three-state) ---
    {
        let _ = write!(buf, "  ");
        *col += 2;
        let (symbol, label, sym_color, label_color) =
            flash_mode_label(state.settings.flash, theme);
        render_tristate(
            buf, col, &mut state.menu_click_regions,
            SettingKey::Flash, symbol, label, sym_color, label_color,
        );
    }

    // --- Elapsed time (bool) ---
    {
        let _ = write!(buf, "  ");
        *col += 2;
        let level = if state.settings.elapsed_time { 2 } else { 0 };
        let (symbol, sym_color, label_color) = tristate_colors(theme, level);
        let label = if state.settings.elapsed_time { "Elapsed time: on" } else { "Elapsed time: off" };
        render_tristate(
            buf, col, &mut state.menu_click_regions,
            SettingKey::ElapsedTime, symbol, label, sym_color, label_color,
        );
    }

    // --- Mode indicator (bool) ---
    {
        let _ = write!(buf, "  ");
        *col += 2;
        let level = if state.settings.mode_indicator { 2 } else { 0 };
        let (symbol, sym_color, label_color) = tristate_colors(theme, level);
        let label = if state.settings.mode_indicator { "Mode indicator: on" } else { "Mode indicator: off" };
        render_tristate(
            buf, col, &mut state.menu_click_regions,
            SettingKey::ModeIndicator, symbol, label, sym_color, label_color,
        );
    }

    // Close button
    let _ = write!(buf, "  ");
    *col += 2;
    let close_start = *col;
    let _ = write!(buf, "{}×", fg(theme.red));
    *col += 1;

    state.menu_click_regions.push(MenuClickRegion {
        start_col: close_start,
        end_col: *col,
        action: MenuAction::CloseMenu,
    });
}
