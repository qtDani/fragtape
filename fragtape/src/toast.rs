//! In-game style notifications: small cards that slide in at the top-right corner of
//! the game's window (following it if it moves), stay on top, and never take focus
//! away from the game. Without a usable game window they go to the monitor's corner.
//!
//!   ask(..)   "New game: X. What should Fragtape record?" with three buttons
//!   info(..)  "Clip saved", "Highlight marked", ... (hides itself after a few seconds)
//!
//! Each card runs its own tiny message loop thread, drawn with plain GDI.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForMonitor;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::win::wide;

const WM_MOUSELEAVE: u32 = 0x02A3;
const ASK_SECONDS: f64 = 30.0;
const SLIDE: f64 = 0.18;

/// (label, sub-label, answer)
const CHOICES: [(&str, &str, &str); 3] =
    [("Record", "whole session", "full"), ("Highlights", "only clips", "highlights"), ("Nothing", "ignore game", "off")];

type OnChoice = Box<dyn FnOnce(&'static str) + Send>;

struct Card {
    kicker: String,
    title: String,
    text: String,
    letter: String,
    icon: HICON,
    ask: bool,
    on_choice: Option<OnChoice>,
    hover: i32, // -1 none, 0..2 buttons, 3 close
    tracking: bool,
    born: Instant,
    paused: Duration,
    pause_start: Option<Instant>,
    life: f64,
    closing: Option<Instant>,
    scale: f64,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    pids: Vec<u32>,
    anchor: isize,  // the game's window
    offset: i32,    // below cards that were already showing
    margin: i32,
    placed: Instant,
}

/// A corner inside the game's window (client area), per the "card_corner" setting; the
/// monitor's corner if the window isn't there (yet), is minimized, or is too small for the card.
/// `offset` stacks cards away from the corner (down from the top, up from the bottom).
unsafe fn place(pids: &[u32], anchor: &mut isize, w: i32, h: i32, margin: i32, offset: i32) -> (i32, i32) {
    let corner = look().corner;
    let at = |left: i32, top: i32, right: i32, bottom: i32| {
        let x = if corner.ends_with("left") { left + margin } else { right - w - margin };
        let y = if corner.starts_with("bottom") { bottom - h - margin - offset } else { top + margin + offset };
        (x, y)
    };
    let mut hw = *anchor as HWND;
    if hw.is_null() || IsWindow(hw) == 0 {
        hw = crate::win::main_window(pids).map(|m| m.hwnd as HWND).unwrap_or(std::ptr::null_mut());
        *anchor = hw as isize;
    }
    if !hw.is_null() && IsIconic(hw) == 0 {
        let mut r: RECT = std::mem::zeroed();
        let mut tl = POINT { x: 0, y: 0 };
        if GetClientRect(hw, &mut r) != 0 && ClientToScreen(hw, &mut tl) != 0
            && r.right >= w + 2 * margin && r.bottom >= h + 2 * margin + offset
        {
            return at(tl.x, tl.y, tl.x + r.right, tl.y + r.bottom);
        }
    }
    let work = crate::win::monitor_work_area(pids);
    at(work.left, work.top, work.right, work.bottom)
}

/// Which way the card slides in: from the nearest side edge.
fn slide_sign() -> f64 {
    if look().corner.ends_with("left") { -1.0 } else { 1.0 }
}

static STACK: OnceLock<Mutex<Vec<(isize, i32)>>> = OnceLock::new();

fn stack() -> &'static Mutex<Vec<(isize, i32)>> {
    STACK.get_or_init(Mutex::default)
}

fn s(c: &Card, v: f64) -> i32 {
    (v * c.scale).round() as i32
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

fn button_rect(c: &Card, i: i32) -> RECT {
    let pad = 14.0;
    let gap = 8.0;
    let bw = (c.w as f64 / c.scale - pad * 2.0 - gap * 2.0) / 3.0;
    let x = pad + i as f64 * (bw + gap);
    RECT { left: s(c, x), top: s(c, 92.0), right: s(c, x + bw), bottom: s(c, 134.0) }
}

fn close_rect(c: &Card) -> RECT {
    RECT { left: c.w - s(c, 30.0), top: s(c, 8.0), right: c.w - s(c, 8.0), bottom: s(c, 30.0) }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

unsafe fn font(c: &Card, px: f64, bold: bool) -> HFONT {
    CreateFontW(-s(c, px), 0, 0, 0, if bold { 600 } else { 400 }, 0, 0, 0, 1, 0, 0, 5 /*CLEARTYPE*/, 0,
                wide("Segoe UI").as_ptr())
}

unsafe fn text(dc: HDC, c: &Card, t: &str, r: RECT, px: f64, bold: bool, color: COLORREF, flags: u32) {
    let f = font(c, px, bold);
    let old = SelectObject(dc, f);
    SetTextColor(dc, color);
    let mut r = r;
    let w: Vec<u16> = t.encode_utf16().collect();
    DrawTextW(dc, w.as_ptr(), w.len() as i32, &mut r, flags | DT_NOPREFIX | DT_END_ELLIPSIS);
    SelectObject(dc, old);
    DeleteObject(f);
}

unsafe fn fill(dc: HDC, r: RECT, color: COLORREF) {
    let b = CreateSolidBrush(color);
    FillRect(dc, &r, b);
    DeleteObject(b);
}

fn remaining(c: &Card) -> f64 {
    let paused = c.paused + c.pause_start.map(|p| p.elapsed()).unwrap_or_default();
    (c.life - (c.born.elapsed().saturating_sub(paused)).as_secs_f64()).max(0.0)
}

/// Colours of the in-game cards, per theme (matches the window's themes).
struct Palette {
    bg: (COLORREF, COLORREF),      // top, bottom (same = flat)
    gloss: bool,                   // Vista: shine over the top half, glossy buttons
    border: Option<COLORREF>,
    text: COLORREF,
    muted: COLORREF,
    kicker: COLORREF,
    accent: COLORREF,
    stripe: i32,                   // width of the accent stripe on the left (0 = none)
    btn: (COLORREF, COLORREF, COLORREF), // background, hover background, border
    btn_text: COLORREF,
    prim: (COLORREF, COLORREF, COLORREF),
    prim_text: COLORREF,
    radius: i32,
    letter_bg: COLORREF,
    progress: COLORREF,
}

/// The appearance settings the cards follow.
#[derive(Clone)]
struct Look {
    theme: String,
    accent: Option<(u8, u8, u8)>, // None = the theme's own
    corner: String,               // top_right, top_left, bottom_right, bottom_left
    info_seconds: f64,
    animate: bool,
}

static LOOK: Mutex<Option<Look>> = Mutex::new(None);

fn look() -> Look {
    LOOK.lock().unwrap().clone().unwrap_or(Look { theme: "sharp".into(), accent: None, corner: "top_right".into(),
                                                  info_seconds: 3.5, animate: true })
}

/// The accent colours offered in Settings > Appearance (the same list as the window's).
pub fn accent_rgb(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "red" => (239, 68, 68), "orange" => (249, 115, 22), "yellow" => (234, 179, 8), "green" => (34, 197, 94),
        "teal" => (20, 184, 166), "blue" => (59, 130, 246), "purple" => (139, 92, 246), "pink" => (236, 72, 153),
        _ => return None,
    })
}

/// Called at start and whenever an appearance setting changes.
pub fn set_look(p: &serde_json::Map<String, serde_json::Value>) {
    let st = |k: &str, d: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
    let info_seconds = match st("card_time", "normal").as_str() { "short" => 2.0, "long" => 6.0, _ => 3.5 };
    *LOOK.lock().unwrap() = Some(Look { theme: st("theme", "sharp"), accent: accent_rgb(&st("accent", "theme")),
        corner: st("card_corner", "top_right"), info_seconds, animate: p.get("animations") != Some(&serde_json::json!(false)) });
}

fn palette() -> Palette {
    let l = look();
    let mut pal = theme_palette(&l.theme);
    if let Some((r, g, b)) = l.accent {
        let a = rgb(r, g, b);
        let lighter = mix(a, rgb(255, 255, 255), 0.15);
        if pal.letter_bg == pal.accent { pal.letter_bg = a; }
        if pal.progress == pal.accent { pal.progress = a; }
        pal.kicker = mix(a, if pal.text == rgb(0, 0, 0) { rgb(0, 0, 0) } else { rgb(255, 255, 255) }, 0.3);
        pal.accent = a;
        pal.prim = (a, lighter, if pal.gloss { mix(a, rgb(0, 0, 0), 0.6) } else { a });
        // dark text on a light accent
        if (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000 > 160 { pal.prim_text = rgb(20, 20, 20); }
    }
    pal
}

fn theme_palette(t: &str) -> Palette {
    match t {
        "win10dark" => Palette { bg: (rgb(43, 43, 43), rgb(43, 43, 43)), gloss: false, border: Some(rgb(61, 61, 61)), text: rgb(255, 255, 255),
            muted: rgb(160, 160, 160), kicker: rgb(76, 163, 232), accent: rgb(0, 120, 212), stripe: 0,
            btn: (rgb(58, 58, 58), rgb(70, 70, 70), rgb(61, 61, 61)), btn_text: rgb(255, 255, 255),
            prim: (rgb(0, 120, 212), rgb(16, 132, 224), rgb(0, 120, 212)), prim_text: rgb(255, 255, 255), radius: 0,
            letter_bg: rgb(0, 120, 212), progress: rgb(0, 120, 212) },
        "metro" => Palette { bg: (rgb(29, 29, 29), rgb(29, 29, 29)), gloss: false, border: None, text: rgb(255, 255, 255),
            muted: rgb(154, 154, 154), kicker: rgb(255, 107, 116), accent: rgb(232, 17, 35), stripe: 4,
            btn: (rgb(42, 42, 42), rgb(56, 56, 56), rgb(42, 42, 42)), btn_text: rgb(255, 255, 255),
            prim: (rgb(232, 17, 35), rgb(245, 40, 56), rgb(232, 17, 35)), prim_text: rgb(255, 255, 255), radius: 0,
            letter_bg: rgb(232, 17, 35), progress: rgb(232, 17, 35) },
        "win10light" => Palette { bg: (rgb(255, 255, 255), rgb(255, 255, 255)), gloss: false, border: Some(rgb(209, 209, 209)), text: rgb(0, 0, 0),
            muted: rgb(92, 92, 92), kicker: rgb(0, 99, 177), accent: rgb(0, 120, 212), stripe: 0,
            btn: (rgb(242, 242, 242), rgb(229, 229, 229), rgb(209, 209, 209)), btn_text: rgb(0, 0, 0),
            prim: (rgb(0, 120, 212), rgb(16, 132, 224), rgb(0, 120, 212)), prim_text: rgb(255, 255, 255), radius: 0,
            letter_bg: rgb(0, 120, 212), progress: rgb(0, 120, 212) },
        "vista" => Palette { bg: (rgb(78, 100, 124), rgb(14, 20, 30)), gloss: true, border: Some(rgb(0, 0, 0)), text: rgb(255, 255, 255),
            muted: rgb(194, 207, 220), kicker: rgb(159, 220, 255), accent: rgb(74, 163, 230), stripe: 0,
            btn: (rgb(69, 76, 85), rgb(90, 98, 108), rgb(11, 11, 11)), btn_text: rgb(255, 255, 255),
            prim: (rgb(74, 163, 230), rgb(96, 180, 240), rgb(8, 49, 94)), prim_text: rgb(255, 255, 255), radius: 3,
            letter_bg: rgb(160, 24, 24), progress: rgb(63, 198, 58) },
        _ => Palette { bg: (rgb(21, 21, 21), rgb(21, 21, 21)), gloss: false, border: Some(rgb(51, 51, 51)), text: rgb(234, 234, 234),
            muted: rgb(138, 138, 138), kicker: rgb(248, 113, 113), accent: rgb(239, 68, 68), stripe: 0,
            btn: (rgb(31, 31, 31), rgb(42, 42, 42), rgb(51, 51, 51)), btn_text: rgb(234, 234, 234),
            prim: (rgb(239, 68, 68), rgb(248, 90, 90), rgb(239, 68, 68)), prim_text: rgb(255, 255, 255), radius: 2,
            letter_bg: rgb(34, 34, 34), progress: rgb(239, 68, 68) },
    }
}

fn mix(a: COLORREF, b: COLORREF, t: f64) -> COLORREF {
    let ch = |c: COLORREF, sh: u32| ((c >> sh) & 0xff) as f64;
    let m = |sh: u32| ((ch(a, sh) * (1.0 - t) + ch(b, sh) * t).round() as u32) << sh;
    m(0) | m(8) | m(16)
}

/// Vertical gradient, one line at a time.
unsafe fn vgrad(dc: HDC, r: RECT, top: COLORREF, bottom: COLORREF) {
    let h = (r.bottom - r.top).max(1);
    for y in 0..h {
        fill(dc, RECT { left: r.left, top: r.top + y, right: r.right, bottom: r.top + y + 1 }, mix(top, bottom, y as f64 / h as f64));
    }
}

/// A button: flat, or Vista-style glossy (light top half, darker bottom half).
unsafe fn button(dc: HDC, c: &Card, pal: &Palette, r: RECT, (bg, border): (COLORREF, COLORREF)) {
    let rad = s(c, pal.radius as f64 * 2.0);
    let rgn = CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, rad + 1, rad + 1);
    SelectClipRgn(dc, rgn);
    if pal.gloss {
        let white = rgb(255, 255, 255);
        let mid = (r.top + r.bottom) / 2;
        vgrad(dc, RECT { bottom: mid, ..r }, mix(bg, white, 0.55), mix(bg, white, 0.18));
        vgrad(dc, RECT { top: mid, ..r }, mix(bg, rgb(0, 0, 0), 0.3), mix(bg, white, 0.12));
    } else {
        fill(dc, r, bg);
    }
    SelectClipRgn(dc, std::ptr::null_mut());
    DeleteObject(rgn);
    let p = CreatePen(PS_SOLID, 1, border);
    let (op, ob) = (SelectObject(dc, p), SelectObject(dc, GetStockObject(NULL_BRUSH)));
    RoundRect(dc, r.left, r.top, r.right, r.bottom, rad, rad);
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(p);
}

unsafe fn paint(hwnd: HWND, c: &Card) {
    let pal = palette();
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let screen = BeginPaint(hwnd, &mut ps);
    let dc = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, c.w, c.h);
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as i32);
    let all = RECT { left: 0, top: 0, right: c.w, bottom: c.h };
    vgrad(dc, all, pal.bg.0, pal.bg.1);
    if pal.gloss {
        // the glass shine over the top part
        let white = rgb(255, 255, 255);
        let shine = RECT { bottom: (c.h as f64 * 0.42) as i32, ..all };
        vgrad(dc, shine, mix(pal.bg.0, white, 0.35), mix(pal.bg.0, white, 0.05));
    }
    if pal.stripe > 0 {
        fill(dc, RECT { left: 0, top: 0, right: s(c, pal.stripe as f64), bottom: c.h }, pal.accent);
    }
    if let Some(b) = pal.border {
        let pen = CreatePen(PS_SOLID, 1, b);
        let (op, ob) = (SelectObject(dc, pen), SelectObject(dc, GetStockObject(NULL_BRUSH)));
        Rectangle(dc, 0, 0, c.w, c.h);
        if pal.gloss {
            let hl = CreatePen(PS_SOLID, 1, rgb(140, 160, 180)); // inner highlight line
            SelectObject(dc, hl);
            Rectangle(dc, 1, 1, c.w - 1, c.h - 1);
            SelectObject(dc, pen);
            DeleteObject(hl);
        }
        SelectObject(dc, op);
        SelectObject(dc, ob);
        DeleteObject(pen);
    }
    // countdown bar
    let frac = remaining(c) / c.life;
    fill(dc, RECT { left: s(c, 2.0), top: c.h - s(c, 4.0), right: s(c, 2.0) + ((c.w - s(c, 4.0)) as f64 * frac) as i32, bottom: c.h - s(c, 2.0) }, pal.progress);
    // icon (or a letter)
    let icon_rect = RECT { left: s(c, 16.0), top: s(c, 16.0), right: s(c, 56.0), bottom: s(c, 56.0) };
    if !c.icon.is_null() {
        DrawIconEx(dc, icon_rect.left, icon_rect.top, c.icon, s(c, 40.0), s(c, 40.0), 0, std::ptr::null_mut(), DI_NORMAL);
    } else {
        button(dc, c, &pal, icon_rect, (pal.letter_bg, pal.letter_bg));
        text(dc, c, &c.letter, icon_rect, 18.0, true, rgb(255, 255, 255), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    let tx = s(c, 68.0);
    let right = c.w - s(c, 34.0);
    text(dc, c, &c.kicker, RECT { left: tx, top: s(c, 13.0), right, bottom: s(c, 29.0) }, 10.5, true, pal.kicker, DT_SINGLELINE);
    text(dc, c, &c.title, RECT { left: tx, top: s(c, 28.0), right, bottom: s(c, 50.0) }, 15.5, true, pal.text, DT_SINGLELINE);
    text(dc, c, &c.text, RECT { left: tx, top: s(c, 51.0), right: c.w - s(c, 14.0), bottom: s(c, 70.0) }, 12.0, false, pal.muted, DT_SINGLELINE);
    // close ×
    let cr = close_rect(c);
    if c.hover == 3 {
        button(dc, c, &pal, cr, (pal.btn.1, pal.btn.1));
    }
    text(dc, c, "×", cr, 16.0, false, pal.muted, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    if c.ask {
        for (i, (label, sub, _)) in CHOICES.iter().enumerate() {
            let r = button_rect(c, i as i32);
            let hovered = c.hover == i as i32;
            let ((bg, hov, border), fg) = if i == 0 { (pal.prim, pal.prim_text) } else { (pal.btn, pal.btn_text) };
            button(dc, c, &pal, r, (if hovered { hov } else { bg }, border));
            let mid = (r.top + r.bottom) / 2;
            text(dc, c, label, RECT { left: r.left, top: r.top + s(c, 3.0), right: r.right, bottom: mid + s(c, 2.0) }, 12.5, true,
                 fg, DT_CENTER | DT_BOTTOM | DT_SINGLELINE);
            text(dc, c, sub, RECT { left: r.left, top: mid + s(c, 2.0), right: r.right, bottom: r.bottom - s(c, 2.0) }, 10.5, false,
                 if i == 0 { mix(fg, pal.prim.0, 0.25) } else { pal.muted }, DT_CENTER | DT_TOP | DT_SINGLELINE);
        }
    }
    BitBlt(screen, 0, 0, c.w, c.h, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    EndPaint(hwnd, &ps);
}

unsafe fn card<'a>(hwnd: HWND) -> Option<&'a mut Card> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Card;
    p.as_mut()
}

fn start_close(c: &mut Card) {
    if c.closing.is_none() {
        c.closing = Some(Instant::now());
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(c) = card(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
    match msg {
        WM_PAINT => { paint(hwnd, c); 0 }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT, // never steal focus from the game
        WM_MOUSEMOVE => {
            let (x, y) = ((lp & 0xffff) as i16 as i32, ((lp >> 16) & 0xffff) as i16 as i32);
            let hover = if inside(&close_rect(c), x, y) { 3 } else if c.ask {
                (0..3).find(|i| inside(&button_rect(c, *i), x, y)).unwrap_or(-1)
            } else { -1 };
            if c.pause_start.is_none() {
                c.pause_start = Some(Instant::now()); // hovering pauses the countdown
            }
            if !c.tracking {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut t);
                c.tracking = true;
            }
            if hover != c.hover {
                c.hover = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            c.tracking = false;
            c.hover = -1;
            if let Some(p) = c.pause_start.take() {
                c.paused += p.elapsed();
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(std::ptr::null_mut(), if c.hover >= 0 { IDC_HAND } else { IDC_ARROW }));
            1
        }
        WM_LBUTTONUP => {
            if c.hover == 3 {
                start_close(c);
            } else if c.ask && (0..3).contains(&c.hover) {
                if let Some(f) = c.on_choice.take() {
                    f(CHOICES[c.hover as usize].2);
                }
                start_close(c);
            } else if !c.ask {
                start_close(c);
            }
            0
        }
        WM_TIMER => {
            if c.placed.elapsed() > Duration::from_millis(250) {
                // follow the game window if it moves or resizes
                c.placed = Instant::now();
                (c.x, c.y) = place(&c.pids, &mut c.anchor, c.w, c.h, c.margin, c.offset);
            }
            let age = c.born.elapsed().as_secs_f64();
            let anim = look().animate;
            let (alpha, dx) = if let Some(t) = c.closing {
                let k = if anim { (t.elapsed().as_secs_f64() / 0.15).min(1.0) } else { 1.0 };
                if k >= 1.0 {
                    DestroyWindow(hwnd);
                    return 0;
                }
                (1.0 - k, k * 30.0 * slide_sign())
            } else {
                let k = if anim { (age / SLIDE).min(1.0) } else { 1.0 };
                let ease = 1.0 - (1.0 - k).powi(3);
                (ease, (1.0 - ease) * 40.0 * slide_sign())
            };
            SetLayeredWindowAttributes(hwnd, 0, (alpha * 248.0) as u8, LWA_ALPHA);
            SetWindowPos(hwnd, HWND_TOPMOST, c.x + s(c, dx), c.y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            if c.closing.is_none() && remaining(c) <= 0.0 {
                start_close(c);
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_CLOSE => { start_close(c); 0 }
        WM_DESTROY => {
            stack().lock().unwrap().retain(|(h, _)| *h != hwnd as isize);
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut Card;
            if !p.is_null() {
                let card = Box::from_raw(p);
                if !card.icon.is_null() {
                    DestroyIcon(card.icon);
                }
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn register_class() -> Vec<u16> {
    static ONCE: OnceLock<()> = OnceLock::new();
    let name = wide("FragtapeToast");
    ONCE.get_or_init(|| unsafe {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = GetModuleHandleW(std::ptr::null());
        wc.lpszClassName = name.as_ptr();
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        RegisterClassExW(&wc);
    });
    name
}

/// HICON from .ico bytes (as made by icons::exe_icon); null for jpg/png.
fn hicon_from(ico: Option<Vec<u8>>) -> HICON {
    match ico {
        Some(d) if d.len() > 22 && d[..4] == [0, 0, 1, 0] => unsafe {
            let size = d[6] as i32;
            CreateIconFromResourceEx(d[22..].as_ptr(), (d.len() - 22) as u32, 1, 0x0003_0000, size, size, LR_DEFAULTCOLOR)
        },
        _ => std::ptr::null_mut(),
    }
}

struct Spec {
    letter: String, // first letter of the game, shown when there's no icon
    kicker: String,
    title: String,
    text: String,
    icon: Option<Vec<u8>>,
    ask: bool,
    on_choice: Option<OnChoice>,
    pids: Vec<u32>,
}

fn show(spec: Spec) -> Arc<AtomicIsize> {
    let handle = Arc::new(AtomicIsize::new(0));
    let h2 = handle.clone();
    std::thread::Builder::new().name("toast".into()).spawn(move || unsafe {
        let class = register_class();
        // Size for the DPI of the monitor the game is on.
        let mut anchor = crate::win::main_window(&spec.pids).map(|m| m.hwnd).unwrap_or(0);
        let mon = if anchor != 0 {
            MonitorFromWindow(anchor as HWND, MONITOR_DEFAULTTONEAREST)
        } else {
            let work = crate::win::monitor_work_area(&spec.pids);
            MonitorFromPoint(POINT { x: work.left + 1, y: work.top + 1 }, MONITOR_DEFAULTTOPRIMARY)
        };
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        GetDpiForMonitor(mon, 0, &mut dpi_x, &mut dpi_y);
        let scale = dpi_x as f64 / 96.0;
        let (w, h) = ((380.0 * scale) as i32, ((if spec.ask { 150.0 } else { 78.0 }) * scale) as i32);
        let margin = (16.0 * scale) as i32;
        let offset: i32 = stack().lock().unwrap().iter().map(|(_, h)| h + margin / 2).sum();
        let (x, y) = place(&spec.pids, &mut anchor, w, h, margin, offset);
        let c = Box::new(Card {
            letter: spec.letter, kicker: spec.kicker, title: spec.title, text: spec.text, icon: hicon_from(spec.icon), ask: spec.ask,
            on_choice: spec.on_choice, hover: -1, tracking: false, born: Instant::now(), paused: Duration::ZERO,
            pause_start: None, life: if spec.ask { ASK_SECONDS } else { look().info_seconds }, closing: None, scale, w, h, x, y,
            pids: spec.pids, anchor, offset, margin, placed: Instant::now(),
        });
        let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED, class.as_ptr(),
                                   wide("Fragtape").as_ptr(), WS_POPUP, x + (40.0 * scale * slide_sign()) as i32, y, w, h,
                                   std::ptr::null_mut(), std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null());
        if hwnd.is_null() {
            return crate::logf!("[toast] could not create the card (error {})", GetLastError());
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(c) as isize);
        // Windows 11 corners: rounded for Vista Aero, square for the other themes.
        let round: u32 = if look().theme == "vista" { 2 } else { 1 }; // DWMWCP_ROUND / DONOTROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const u32 as *const _, 4);
        SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE); // never in a screen recording
        stack().lock().unwrap().push((hwnd as isize, h));
        h2.store(hwnd as isize, Ordering::Relaxed);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, 1, 15, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }).ok();
    handle
}

/// Ask what to record for a new game. `on_choice` gets "full" / "highlights" / "off".
/// Returns a function that closes the card (when the question was answered elsewhere).
pub fn ask(game: &str, icon: Option<Vec<u8>>, pids: Vec<u32>, on_choice: OnChoice) -> Box<dyn FnOnce() + Send> {
    let handle = show(Spec {
        letter: initial(game), kicker: "NEW GAME DETECTED".into(), title: game.into(), text: "What should Fragtape record?".into(),
        icon, ask: true, on_choice: Some(on_choice), pids,
    });
    Box::new(move || close(&handle))
}

fn initial(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string()
}

/// A notice that hides itself after a few seconds. `game` gives the letter shown without an icon.
pub fn info(kicker: &str, title: &str, text: &str, game: &str, icon: Option<Vec<u8>>, pids: Vec<u32>) {
    show(Spec { letter: initial(game), kicker: kicker.into(), title: title.into(), text: text.into(), icon, ask: false, on_choice: None, pids });
}

fn close(handle: &AtomicIsize) {
    let h = handle.load(Ordering::Relaxed);
    if h != 0 {
        unsafe { PostMessageW(h as HWND, WM_CLOSE, 0, 0) };
    }
}
