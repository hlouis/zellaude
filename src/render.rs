use crate::state::{
    unix_now, unix_now_ms, Activity, ClickRegion, FlashMode, MenuAction, MenuClickRegion,
    NotifyMode, SessionInfo, SettingKey, State, ViewMode,
};
use crate::theme::{readable, Rgb, Theme};
use std::cmp::Reverse;
use std::fmt::Write;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
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
                // Every status symbol must occupy exactly one terminal column,
                // or a session flipping between states makes its tab grow and
                // shrink, shifting every tab to its right. ⚡ (U+26A1) was the
                // lone offender: Emoji_Presentation=Yes and EAW=Wide, so it is
                // two columns everywhere. ❖ is single-column.
                "Bash" => "❖",
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

/// Terminal columns a string occupies — not its char count.
///
/// CJK tab names and the ⚡ tool symbol are double-width, so counting chars
/// under-reports and the bar silently overruns `cols` (it clips rather than
/// wraps, so the rightmost tabs vanish early). Ambiguous-width characters are
/// treated as narrow, matching a non-CJK terminal; a terminal configured to
/// render them wide would still drift.
fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut `s` down to at most `max_cols` terminal columns, ending in … if cut.
/// Never splits a double-width char across the boundary — it stops short and
/// leaves the column unused rather than emitting half a glyph.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    let budget = max_cols.saturating_sub(1); // one column for the …
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ELAPSED_THRESHOLD: u64 = 30;
/// Most status icons drawn per tab; beyond this the least urgent are collapsed
/// into a single ellipsis so a busy tab can't starve every tab name of width.
const MAX_ICONS: usize = 4;
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

    // Every Claude session in each tab, as (icons to draw, some were dropped).
    //
    // Selection and display use different orders on purpose: we keep the
    // MAX_ICONS most *urgent* sessions, but draw them in pane_id order. Picking
    // by pane_id would hide a ⚠ sitting on the 5th pane; drawing by priority
    // would make icons jump around as states change. Sorts are stable and the
    // BTreeMap yields pane_id order, so ties stay deterministic.
    let tab_sessions: Vec<(Vec<&SessionInfo>, bool)> = tabs
        .iter()
        .map(|tab| {
            let mut v: Vec<&SessionInfo> = state
                .sessions
                .values()
                .filter(|s| s.tab_index == Some(tab.position))
                .collect();
            let overflow = v.len() > MAX_ICONS;
            if overflow {
                v.sort_by_key(|s| Reverse(activity_priority(&s.activity)));
                v.truncate(MAX_ICONS);
            }
            v.sort_by_key(|s| s.pane_id);
            (v, overflow)
        })
        .collect();

    // The tab's headline session — highest priority, drives elapsed time.
    // Truncation above keeps the top-K by priority, so this is unaffected by it.
    let best_sessions: Vec<Option<&SessionInfo>> = tab_sessions
        .iter()
        .map(|(v, _)| v.iter().copied().max_by_key(|s| activity_priority(&s.activity)))
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
    // Claude tabs: leading space + trailing space + space before name, plus one
    // column per icon and one for the overflow ellipsis. (The old constant 4 was
    // this same formula with exactly one icon.)
    let per_tab_overhead: usize = tab_sessions
        .iter()
        .map(|(v, overflow)| {
            if v.is_empty() {
                2
            } else {
                let icons_w: usize = v
                    .iter()
                    .map(|s| display_width(activity_style(&s.activity, theme).symbol))
                    .sum();
                3 + icons_w + usize::from(*overflow)
            }
        })
        .sum();
    // Arrows: 1 for the first tab, 2 for each later one, 1 closing back to the
    // bar background — exactly 2 * count.
    let overhead = prefix_width + 2 * count + per_tab_overhead + total_elapsed_width;
    let max_name_len = if overhead < cols {
        ((cols - overhead) / count).min(20)
    } else {
        0
    };

    let mut prev_bg = prefix_bg;

    for (i, tab) in tabs.iter().enumerate() {
        // Stop if we'd overflow — need room for this tab's arrow(s) plus the
        // closing arrow back to the bar background.
        let arrows_needed = if i == 0 { 1 } else { 2 };
        if *col + arrows_needed + 3 > cols {
            break;
        }

        let (icons, icons_overflow) = &tab_sessions[i];
        let is_claude = !icons.is_empty();
        let tab_name = &tab.name;

        // Truncate name to the column budget (not a char count — see display_width)
        let truncated = truncate_to_width(tab_name, max_name_len);

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

        // First tab sits flush against the prefix; every later tab is preceded
        // by a blank column (arrow out to the bar background, then back in).
        //
        // This keys off the tab's position, not its color. The old test was
        // `prev_bg == prefix_bg`, meant as "is this the first tab" — but
        // tab_inactive_bg and prefix_bg are both ribbon_unselected, i.e. the
        // same color, so it really asked "did an inactive tab precede me" and
        // the blank column landed after active tabs instead.
        if i == 0 {
            arrow(buf, col, prev_bg, tab_bg);
        } else {
            arrow(buf, col, prev_bg, theme.bar_bg);
            arrow(buf, col, theme.bar_bg, tab_bg);
        }

        let tab_bg_str = bg(tab_bg);
        let region_start = *col;

        if is_claude {
            let (name_fg, name_bold) = if is_flash_bright {
                (fg(theme.flash_text), true)
            } else if is_active {
                (fg(theme.text_active), true)
            } else {
                (fg(theme.text_inactive), false)
            };

            // Leading space
            let _ = write!(buf, "{tab_bg_str} ");
            *col += 1;

            // One symbol per session in this tab. No separator between them:
            // each carries its own color, and they read as one cluster — the
            // tab's state. A space each would cost n-1 columns for nothing.
            for s in icons {
                if *col + 2 > cols {
                    break;
                }
                let style = activity_style(&s.activity, theme);
                let sym_fg = if is_flash_bright {
                    fg(theme.flash_text)
                } else {
                    // Accents are tuned for the bar background; retune to the
                    // tab's, or a light-ribbon theme swallows them.
                    fg(readable(style.color, tab_bg))
                };
                let _ = write!(buf, "{sym_fg}{}", style.symbol);
                *col += display_width(style.symbol);
            }

            // Overflow marker — reuses the name-truncation vocabulary and costs
            // one column, where a "+N" would cost as much as just drawing them.
            if *icons_overflow && *col + 2 <= cols {
                let _ = write!(buf, "{}…", fg(theme.text_inactive));
                *col += 1;
            }

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
                let _ = write!(buf, " {}F{RESET}{tab_bg_str}", fg(readable(theme.yellow, tab_bg)));
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
                let _ = write!(buf, " {}F{RESET}{tab_bg_str}", fg(readable(theme.yellow, tab_bg)));
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
