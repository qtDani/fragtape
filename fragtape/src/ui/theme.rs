//! The five themes and the appearance settings, applied to the Slint Palette.

use serde_json::{Map, Value};
use slint::{Color, ComponentHandle};

use super::{AppWindow, Palette};

fn c(hex: u32) -> Color {
    Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}
fn ca(hex: u32, a: f32) -> Color {
    Color::from_argb_u8((a * 255.0) as u8, (hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

struct Theme {
    bg: Color, side: Color, panel: Color, card: Color, card_hi: Color, line: Color, hover_line: Color,
    text: Color, muted: Color, faint: Color, accent: Color, sel: Color, bar: f32, track: Color,
    r_sm: f32, r_lg: f32, gloss: bool, blocks: bool, light: bool,
}

fn theme(name: &str) -> Theme {
    match name {
        "win10dark" => Theme { bg: c(0x1f1f1f), side: c(0x191919), panel: c(0x262626), card: c(0x2b2b2b), card_hi: c(0x353535),
            line: c(0x3d3d3d), hover_line: c(0x5a5a5a), text: c(0xffffff), muted: c(0xa0a0a0), faint: c(0x7a7a7a), accent: c(0x0078d4),
            sel: c(0x2d2d2d), bar: 4.0, track: c(0x4a4a4a), r_sm: 0.0, r_lg: 0.0, gloss: false, blocks: false, light: false },
        "metro" => Theme { bg: c(0x111111), side: c(0x111111), panel: c(0x1a1a1a), card: c(0x1d1d1d), card_hi: c(0x262626),
            line: c(0x1d1d1d), hover_line: c(0xe81123), text: c(0xffffff), muted: c(0x9a9a9a), faint: c(0x777777), accent: c(0xe81123),
            sel: c(0xe81123), bar: 0.0, track: c(0x333333), r_sm: 0.0, r_lg: 0.0, gloss: false, blocks: true, light: false },
        "win10light" => Theme { bg: c(0xffffff), side: c(0xf2f2f2), panel: c(0xf9f9f9), card: c(0xf2f2f2), card_hi: c(0xe5e5e5),
            line: c(0xd1d1d1), hover_line: c(0x0078d4), text: c(0x000000), muted: c(0x5c5c5c), faint: c(0x767676), accent: c(0x0078d4),
            sel: c(0xe5e5e5), bar: 4.0, track: c(0xc8c8c8), r_sm: 0.0, r_lg: 0.0, gloss: false, blocks: false, light: true },
        "vista" => Theme { bg: Color::from_argb_u8(0, 0, 0, 0), panel: ca(0x0c1016, 0.72), card: ca(0x28303a, 0.75), card_hi: ca(0x465464, 0.8),
            side: ca(0x324660, 0.5), line: ca(0x000000, 0.85), hover_line: ca(0x78c8ff, 0.8), text: c(0xffffff), muted: c(0xb8c6d4), faint: c(0x8796a6),
            accent: c(0x3b9ae8), sel: Color::from_argb_u8(0, 0, 0, 0), bar: 0.0, track: c(0x1a1f25), r_sm: 3.0, r_lg: 6.0, gloss: true, blocks: false, light: false },
        _ => Theme { bg: c(0x0d0d0d), side: c(0x0d0d0d), panel: c(0x121212), card: c(0x171717), card_hi: c(0x222222),
            line: c(0x2a2a2a), hover_line: c(0x444444), text: c(0xeaeaea), muted: c(0x8a8a8a), faint: c(0x666666), accent: c(0xef4444),
            sel: c(0x1a1a1a), bar: 2.0, track: c(0x333333), r_sm: 2.0, r_lg: 2.0, gloss: false, blocks: false, light: false },
    }
}

/// (accent, text on it) for the accent setting; None = the theme's own.
fn accent(name: &str) -> Option<(Color, Color)> {
    let (rgb, dark_text) = match name {
        "red" => (0xef4444, false), "orange" => (0xf97316, false), "yellow" => (0xeab308, true), "green" => (0x22c55e, true),
        "teal" => (0x14b8a6, true), "blue" => (0x3b82f6, false), "purple" => (0x8b5cf6, false), "pink" => (0xec4899, false),
        _ => return None,
    };
    Some((c(rgb), if dark_text { c(0x141414) } else { c(0xffffff) }))
}

pub fn apply(win: &AppWindow, p: &Map<String, Value>) {
    let st = |k: &str, d: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
    let t = theme(&st("theme", "sharp"));
    let g = win.global::<Palette>();
    let (acc, on) = accent(&st("accent", "theme")).unwrap_or((t.accent, c(0xffffff)));
    g.set_bg(t.bg);
    g.set_side_color(t.side);
    g.set_panel(t.panel);
    g.set_card(t.card);
    g.set_card_hi(t.card_hi);
    g.set_line(t.line);
    g.set_hover_line(if t.blocks { acc } else { t.hover_line });
    g.set_text(t.text);
    g.set_muted(t.muted);
    g.set_faint(t.faint);
    g.set_accent(acc);
    g.set_on_accent(on);
    g.set_sel(if t.blocks { acc } else { t.sel });
    g.set_bar(t.bar);
    g.set_track(t.track);
    g.set_r_sm(t.r_sm);
    g.set_r_lg(t.r_lg);
    g.set_gloss(t.gloss);
    g.set_blocks(t.blocks);
    g.set_light(t.light);
    g.set_fs(match st("text_size", "normal").as_str() { "small" => 11.5, "large" => 15.0, _ => 13.0 });
    g.set_thumb(match st("thumb_size", "medium").as_str() { "small" => 190.0, "large" => 340.0, _ => 250.0 });
    let motion = p.get("animations") != Some(&Value::Bool(false));
    g.set_anim(if motion { 120 } else { 0 });
}

/// The little two-colour previews in the theme picker (Vista's gradients are drawn by the UI).
pub fn preview(id: &str) -> (Color, Color) {
    match id {
        "win10dark" => (c(0x191919), c(0x2b2b2b)),
        "metro" => (c(0xe81123), c(0x1d1d1d)),
        "win10light" => (c(0xf2f2f2), c(0xffffff)),
        "vista" => (c(0x4a90c8), c(0x0b2a4a)),
        _ => (c(0x0d0d0d), c(0x151515)),
    }
}

pub fn accent_swatch(id: &str) -> Color {
    accent(id).map(|a| a.0).unwrap_or(c(0x888888))
}
