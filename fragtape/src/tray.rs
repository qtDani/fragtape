//! Notification-area icon, its menu, and the global highlight hotkey (plain Win32).

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::Cmd;
use crate::win::wide;

const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
const WM_QUIT_APP: u32 = WM_APP + 3;
const HOTKEY_ID: i32 = 1;

pub type MenuFn = Box<dyn Fn() -> Vec<(String, Option<Cmd>)> + Send + Sync>;

struct TrayState {
    send: Box<dyn Fn(Cmd) + Send + Sync>,
    menu: MenuFn,
    icon: Mutex<(String, String)>, // (color name, tooltip)
    hotkey: Mutex<(Option<(u32, u32)>, Option<(u32, u32)>)>, // (wanted, registered)
    notify: Mutex<Option<(String, String)>>,
}

static STATE: OnceLock<TrayState> = OnceLock::new();
static HWND_: AtomicIsize = AtomicIsize::new(0);

// ---- icons --------------------------------------------------------------------------------
fn color(name: &str) -> (u8, u8, u8) {
    match name { "rec" => (239, 68, 68), "clip" => (245, 158, 11), "idle" => (150, 155, 170), _ => (34, 197, 94) }
}

/// True when the taskbar uses the light theme (then the tray bomb is drawn dark).
fn light_taskbar() -> bool {
    crate::win::reg_dword(crate::win::HKCU, r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
                          "SystemUsesLightTheme") == Some(1)
}

/// 32-bit icon BMP (header + bottom-up pixels + mask) from top-down RGBA.
fn bmp(px: &[[u8; 4]], size: u32) -> Vec<u8> {
    let mask = vec![0u8; (size.div_ceil(32) * 4 * size) as usize];
    let mut out = Vec::new();
    for v in [40u32, size, size * 2] { out.extend(v.to_le_bytes()); }
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    for v in [0u32, size * size * 4 + mask.len() as u32, 0, 0, 0, 0] { out.extend(v.to_le_bytes()); }
    for row in px.chunks(size as usize).rev() {
        for p in row { out.extend([p[2], p[1], p[0], p[3]]); }
    }
    out.extend(mask);
    out
}

/// The tray icon: a bomb whose spark shows the state.
fn icon_bitmap(spark: (u8, u8, u8), size: u32) -> Vec<u8> {
    let body = if light_taskbar() { (24, 24, 26) } else { (242, 242, 242) };
    bmp(&crate::logo::rgba(size, body, spark, None), size)
}

/// The app icon as a multi-size .ico (for the Start menu shortcut): white bomb on a dark tile.
pub fn ico_file() -> Vec<u8> {
    let sizes = [16u32, 32, 48, 256];
    let imgs: Vec<Vec<u8>> = sizes.iter()
        .map(|&s| bmp(&crate::logo::rgba(s, (242, 242, 242), (255, 138, 28), Some((22, 24, 28))), s))
        .collect();
    let mut ico = vec![0, 0, 1, 0];
    ico.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&imgs) {
        let d = if *s >= 256 { 0 } else { *s as u8 };
        ico.extend([d, d, 0, 0, 1, 0, 32, 0]);
        ico.extend((img.len() as u32).to_le_bytes());
        ico.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in imgs { ico.extend(img); }
    ico
}

fn make_icon(name: &str) -> HICON {
    let bmp = icon_bitmap(color(name), 32);
    unsafe { CreateIconFromResourceEx(bmp.as_ptr(), bmp.len() as u32, 1, 0x0003_0000, 32, 32, LR_DEFAULTCOLOR) }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    for (i, c) in s.encode_utf16().take(dst.len() - 1).enumerate() {
        dst[i] = c;
    }
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

fn apply_icon(hwnd: HWND, msg: u32) {
    let st = STATE.get().unwrap();
    let (name, tip) = st.icon.lock().unwrap().clone();
    let mut n = nid(hwnd);
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = make_icon(&name);
    copy_wide(&mut n.szTip, &tip);
    unsafe {
        Shell_NotifyIconW(msg, &n);
        DestroyIcon(n.hIcon);
    }
}

fn sync_hotkey(hwnd: HWND) {
    let st = STATE.get().unwrap();
    let mut hk = st.hotkey.lock().unwrap();
    if hk.0 == hk.1 {
        return;
    }
    unsafe {
        if hk.1.take().is_some() {
            UnregisterHotKey(hwnd, HOTKEY_ID);
        }
        if let Some((mods, vk)) = hk.0 {
            if RegisterHotKey(hwnd, HOTKEY_ID, mods | MOD_NOREPEAT, vk) != 0 {
                hk.1 = Some((mods, vk));
            } else {
                crate::logf!("[tray] the highlight hotkey is already used by another app");
                hk.0 = None;
            }
        }
    }
}

fn show_menu(hwnd: HWND) {
    let st = STATE.get().unwrap();
    let mut items = vec![("Open Fragtape".to_string(), Some(Cmd::Show))];
    items.extend((st.menu)());
    items.push(("-".into(), None));
    items.push(("Quit Fragtape".into(), Some(Cmd::Quit)));
    unsafe {
        let menu = CreatePopupMenu();
        for (i, (label, cmd)) in items.iter().enumerate() {
            if label == "-" {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            } else {
                AppendMenuW(menu, if cmd.is_some() { MF_STRING } else { MF_STRING | MF_GRAYED }, 100 + i, wide(label).as_ptr());
            }
        }
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if chosen >= 100 {
            if let Some((_, Some(cmd))) = items.get(chosen as usize - 100) {
                (st.send)(cmd.clone());
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
    let taskbar = *TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
    let st = STATE.get().unwrap();
    match msg {
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => (st.send)(Cmd::Show),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_HOTKEY if wp as i32 == HOTKEY_ID => {
            (st.send)(Cmd::Mark);
            0
        }
        WM_REFRESH => {
            apply_icon(hwnd, NIM_MODIFY);
            sync_hotkey(hwnd);
            if let Some((title, text)) = st.notify.lock().unwrap().take() {
                let mut n = nid(hwnd);
                n.uFlags = NIF_INFO;
                copy_wide(&mut n.szInfoTitle, &title);
                copy_wide(&mut n.szInfo, &text);
                Shell_NotifyIconW(NIM_MODIFY, &n);
            }
            0
        }
        WM_QUIT_APP => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            Shell_NotifyIconW(NIM_DELETE, &nid(hwnd));
            PostQuitMessage(0);
            0
        }
        m if m == taskbar => {
            apply_icon(hwnd, NIM_ADD); // Explorer restarted
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn post(msg: u32) {
    let h = HWND_.load(Ordering::Relaxed);
    if h != 0 {
        unsafe { PostMessageW(h as HWND, msg, 0, 0) };
    }
}

// ---- public API (any thread) -------------------------------------------------------------------
pub fn set_state(icon: &str, tip: &str) {
    if let Some(st) = STATE.get() {
        let mut cur = st.icon.lock().unwrap();
        if cur.0 != icon || cur.1 != tip {
            *cur = (icon.into(), tip.into());
            drop(cur);
            post(WM_REFRESH);
        }
    }
}

pub fn set_hotkey(spec: &str) {
    if let Some(st) = STATE.get() {
        st.hotkey.lock().unwrap().0 = parse_hotkey(spec);
        post(WM_REFRESH);
    }
}

pub fn quit() {
    post(WM_QUIT_APP);
}

/// Create the icon and run the message loop on this thread until Quit.
pub fn run(send: Box<dyn Fn(Cmd) + Send + Sync>, menu: MenuFn, hotkey: &str) {
    let _ = STATE.set(TrayState { send, menu, icon: Mutex::new(("watching".into(), "Fragtape".into())),
                                  hotkey: Mutex::new((parse_hotkey(hotkey), None)), notify: Mutex::new(None) });
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("FragtapeTray");
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(0, class.as_ptr(), wide("Fragtape").as_ptr(), 0, 0, 0, 0, 0,
                                   std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null());
        HWND_.store(hwnd as isize, Ordering::Relaxed);
        apply_icon(hwnd, NIM_ADD);
        sync_hotkey(hwnd);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        HWND_.store(0, Ordering::Relaxed);
    }
}

/// "Ctrl+Shift+F8" -> (modifiers, virtual key); "off" -> None.
pub fn parse_hotkey(spec: &str) -> Option<(u32, u32)> {
    if spec.is_empty() || spec.eq_ignore_ascii_case("off") {
        return None;
    }
    let (mut mods, mut vk) = (0u32, None);
    for part in spec.split('+') {
        let p = part.trim().to_lowercase();
        match p.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            f if f.starts_with('f') && f[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) =>
                vk = Some(0x6F + f[1..].parse::<u32>().unwrap()),
            c if c.len() == 1 && c.chars().all(|c| c.is_ascii_alphanumeric()) => vk = Some(c.to_ascii_uppercase().as_bytes()[0] as u32),
            _ => {}
        }
    }
    vk.map(|v| (mods, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys() {
        assert_eq!(parse_hotkey("F8"), Some((0, 0x77)));
        assert_eq!(parse_hotkey("Ctrl+F8"), Some((MOD_CONTROL, 0x77)));
        assert_eq!(parse_hotkey("off"), None);
    }

    /// Writes every tray icon variant to %TEMP%ragtape-icons for a visual check:
    /// cargo test --release dump_icons -- --ignored
    #[test]
    #[ignore]
    fn dump_icons() {
        let dir = std::env::temp_dir().join("fragtape-icons");
        std::fs::create_dir_all(&dir).unwrap();
        let one = |bmp: Vec<u8>, size: u32| {
            let mut ico = vec![0, 0, 1, 0, 1, 0, size as u8, size as u8, 0, 0, 1, 0, 32, 0];
            ico.extend((bmp.len() as u32).to_le_bytes());
            ico.extend(22u32.to_le_bytes());
            ico.extend(bmp);
            ico
        };
        for state in ["rec", "clip", "idle", "watching"] {
            for (taskbar, body) in [("dark", (242, 242, 242)), ("light", (24, 24, 26))] {
                let px = crate::logo::rgba(32, body, color(state), None);
                std::fs::write(dir.join(format!("tray-{state}-{taskbar}.ico")), one(bmp(&px, 32), 32)).unwrap();
            }
        }
        std::fs::write(dir.join("app.ico"), ico_file()).unwrap();
    }

    #[test]
    fn ico_is_valid() {
        let ico = ico_file();
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 4, 0]);
        let last = &ico[6 + 16 * 3..6 + 16 * 4];
        let size = u32::from_le_bytes(last[8..12].try_into().unwrap()) as usize;
        let off = u32::from_le_bytes(last[12..16].try_into().unwrap()) as usize;
        assert_eq!(ico.len(), off + size);
    }
}
