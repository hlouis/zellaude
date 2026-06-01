//! Color theme for the status bar.
//!
//! Every color the bar draws comes from a `Theme`. At runtime the theme is
//! derived from Zellij's own theme (`ModeInfo.style.colors`, a `Styling`), so
//! the bar follows the user's Zellij theme — including automatic dark/light
//! switching (Zellij re-sends `ModeUpdate` with new colors on `toggle-theme`).
//! `Theme::fallback()` is the only hardcoded palette; it is used until the
//! first `ModeUpdate` arrives.

use zellij_tile::prelude::{PaletteColor, Styling};

pub type Rgb = (u8, u8, u8);

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // Structural backgrounds
    pub bar_bg: Rgb,
    pub prefix_bg: Rgb,
    pub prefix_bg_settings: Rgb,
    pub tab_active_bg: Rgb,
    pub tab_inactive_bg: Rgb,
    pub flash_bg: Rgb,
    // Text
    pub text_active: Rgb,   // text on the active tab / prefix
    pub text_inactive: Rgb, // inactive tab name
    pub text_dim: Rgb,      // elapsed suffix
    pub flash_text: Rgb,
    // Semantic accents — the activity and mode hues. These are the only
    // distinct colors; activities/modes pick one each (see render.rs).
    pub green: Rgb,  // done, prompting, NORMAL mode
    pub red: Rgb,    // waiting, LOCKED mode, close button
    pub orange: Rgb, // tools, resize/move modes
    pub yellow: Rgb, // notification, scroll/search/rename modes, fullscreen mark, partial toggle
    pub cyan: Rgb,   // PANE mode
    pub purple: Rgb, // thinking, TAB/SESSION modes
    pub gray: Rgb,   // init, idle, disabled toggles
}

/// Convert a Zellij `PaletteColor` to RGB, expanding 8-bit indices via the
/// standard xterm-256 palette (themes almost always use Rgb, but EightBit is
/// valid and must be handled).
fn to_rgb(c: PaletteColor) -> Rgb {
    match c {
        PaletteColor::Rgb(rgb) => rgb,
        PaletteColor::EightBit(n) => eightbit_to_rgb(n),
    }
}

fn eightbit_to_rgb(n: u8) -> Rgb {
    match n {
        // 16 standard system colors
        0 => (0, 0, 0),
        1 => (205, 0, 0),
        2 => (0, 205, 0),
        3 => (205, 205, 0),
        4 => (0, 0, 238),
        5 => (205, 0, 205),
        6 => (0, 205, 205),
        7 => (229, 229, 229),
        8 => (127, 127, 127),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (92, 92, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        // 6x6x6 color cube
        16..=231 => {
            let i = n - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        // 24-step grayscale ramp
        232..=255 => {
            let v = 8 + (n - 232) * 10;
            (v, v, v)
        }
    }
}

fn lum((r, g, b): Rgb) -> i32 {
    (r as i32 * 299 + g as i32 * 587 + b as i32 * 114) / 1000
}

// --- HSL, matching CSS semantics (H in degrees [0,360), S/L in [0,1]) ---

fn rgb_to_hsl((r, g, b): Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h2 = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h2.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h2 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (f(r1), f(g1), f(b1))
}

/// Synthesize a hue from a theme accent: keep the accent's saturation and
/// lightness (so it matches the theme's vividness and dark/light mood), but
/// force the hue. This is the CSS `hsl(from <accent> <hue> s l)` idea — used to
/// fill in a color Zellij's palette does not expose (a true yellow), without
/// hardcoding RGB. The saturation floor keeps a near-gray accent from
/// producing a washed-out result.
fn with_hue(accent: Rgb, hue: f32) -> Rgb {
    let (_, s, l) = rgb_to_hsl(accent);
    hsl_to_rgb(hue, s.max(0.55), l)
}

impl Theme {
    /// A near-black or near-white that is guaranteed readable on `bg` — used
    /// for text drawn on a colored accent pill (e.g. the mode indicator), where
    /// `bg` comes from an accent and has no paired text color in the theme.
    pub fn on(&self, bg: Rgb) -> Rgb {
        if lum(bg) > 140 {
            (20, 20, 28)
        } else {
            (240, 240, 245)
        }
    }

    /// Derive a theme from Zellij's `Styling`. The bar's UI maps naturally onto
    /// Zellij's ribbon/text/exit-code slots, so this follows the active theme.
    pub fn from_styling(s: &Styling) -> Self {
        let tu = s.text_unselected;
        let rs = s.ribbon_selected;
        let ru = s.ribbon_unselected;
        Theme {
            bar_bg: to_rgb(tu.background),
            prefix_bg: to_rgb(ru.background),
            prefix_bg_settings: to_rgb(rs.background),
            tab_active_bg: to_rgb(rs.background),
            tab_inactive_bg: to_rgb(ru.background),
            flash_bg: to_rgb(s.exit_code_error.background),
            text_active: to_rgb(rs.base),
            text_inactive: to_rgb(ru.base),
            text_dim: to_rgb(tu.base),
            flash_text: to_rgb(s.exit_code_error.base),
            green: to_rgb(s.exit_code_success.base),
            red: to_rgb(s.exit_code_error.base),
            orange: to_rgb(tu.emphasis_0),
            // Zellij exposes no yellow accent — synthesize one from the theme's
            // orange via HSL (hue→yellow, keep saturation/lightness).
            yellow: with_hue(to_rgb(tu.emphasis_0), 50.0),
            cyan: to_rgb(tu.emphasis_1),
            purple: to_rgb(tu.emphasis_3),
            gray: to_rgb(ru.base),
        }
    }

    /// The original hardcoded dark palette. Used until the first `ModeUpdate`.
    pub fn fallback() -> Self {
        Theme {
            bar_bg: (30, 30, 46),
            prefix_bg: (60, 50, 80),
            prefix_bg_settings: (100, 70, 140),
            tab_active_bg: (140, 100, 200),
            tab_inactive_bg: (80, 75, 110),
            flash_bg: (80, 80, 30),
            text_active: (255, 255, 255),
            text_inactive: (120, 220, 220),
            text_dim: (165, 160, 180),
            flash_text: (255, 255, 80),
            green: (80, 200, 120),
            red: (255, 60, 60),
            orange: (255, 170, 50),
            yellow: (230, 205, 90),
            cyan: (80, 180, 255),
            purple: (180, 140, 255),
            gray: (180, 175, 195),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::fallback()
    }
}
