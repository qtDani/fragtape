//! Win32 helpers: processes, windows, monitors, registry, Recycle Bin.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---- processes ---------------------------------------------------------------------
/// Running processes: lower-cased exe name -> pids, with lazy full-path lookup.
#[derive(Default)]
pub struct Snapshot {
    pub pids: HashMap<String, Vec<u32>>,
    paths: Mutex<HashMap<String, Vec<String>>>,
}

impl Snapshot {
    #[cfg(test)]
    pub fn from_names(names: &[&str]) -> Self {
        Snapshot { pids: names.iter().map(|n| (n.to_string(), vec![])).collect(), ..Default::default() }
    }

    #[cfg(test)]
    pub fn with_paths(names: &[(&str, &str)]) -> Self {
        let s = Snapshot { pids: names.iter().map(|(n, _)| (n.to_string(), vec![])).collect(), ..Default::default() };
        {
            let mut p = s.paths.lock().unwrap();
            for (n, path) in names {
                p.entry(n.to_string()).or_default().push(path.to_string());
            }
        }
        s
    }

    pub fn contains(&self, name: &str) -> bool {
        self.pids.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.pids.keys()
    }

    pub fn pids_of(&self, name: &str) -> Vec<u32> {
        self.pids.get(name).cloned().unwrap_or_default()
    }

    /// Lower-cased full paths with "/" separators.
    pub fn paths(&self, name: &str) -> Vec<String> {
        let mut cache = self.paths.lock().unwrap();
        cache
            .entry(name.to_string())
            .or_insert_with(|| self.pids_of(name).into_iter().filter_map(image_path).collect())
            .clone()
    }
}

pub fn running_processes() -> Snapshot {
    let mut pids: HashMap<String, Vec<u32>> = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Snapshot::default();
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            pids.entry(from_wide(&e.szExeFile).to_lowercase()).or_default().push(e.th32ProcessID);
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    Snapshot { pids, ..Default::default() }
}

pub fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..size as usize]).replace('\\', "/").to_lowercase())
    }
}

pub fn lower_thread_priority() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_IDLE);
    }
}

// ---- windows ----------------------------------------------------------------------------
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub class: String,
}

fn enum_windows() -> Vec<(isize, u32)> {
    unsafe extern "system" fn visit(hwnd: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp as *mut Vec<(isize, u32)>);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        out.push((hwnd as isize, pid));
        1
    }
    let mut out: Vec<(isize, u32)> = Vec::new();
    unsafe { EnumWindows(Some(visit), &mut out as *mut _ as LPARAM) };
    out
}

pub fn window_info(hwnd: isize) -> WindowInfo {
    let h = hwnd as HWND;
    let mut title = [0u16; 512];
    let mut class = [0u16; 256];
    unsafe {
        GetWindowTextW(h, title.as_mut_ptr(), 512);
        GetClassNameW(h, class.as_mut_ptr(), 256);
        WindowInfo { hwnd, title: from_wide(&title), class: from_wide(&class) }
    }
}

/// The biggest visible, unowned top-level window of any of `pids`.
pub fn main_window(pids: &[u32]) -> Option<WindowInfo> {
    let mut best: Option<(i64, isize)> = None;
    for (hwnd, pid) in enum_windows() {
        if !pids.contains(&pid) {
            continue;
        }
        let h = hwnd as HWND;
        unsafe {
            if IsWindowVisible(h) == 0 || !GetWindow(h, GW_OWNER).is_null() {
                continue;
            }
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(h, &mut r) == 0 {
                continue;
            }
            let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
            if best.map_or(true, |(a, _)| area > a) {
                best = Some((area, hwnd));
            }
        }
    }
    best.map(|(_, h)| window_info(h))
}

/// Window string used by OBS capture sources: "title:class:exe" with ':' and '#' escaped.
pub fn obs_window_spec(exe: &str, window: Option<&WindowInfo>) -> String {
    let enc = |s: &str| s.replace('#', "#22").replace(':', "#3A");
    let (t, c) = window.map(|w| (w.title.as_str(), w.class.as_str())).unwrap_or(("", ""));
    format!("{}:{}:{}", enc(t), enc(c), enc(exe))
}

/// Post WM_CLOSE to every top-level window of `pid` whose title starts with `prefix`.
pub fn close_windows(pid: u32, prefix: &str) -> bool {
    let mut any = false;
    for (hwnd, p) in enum_windows() {
        if p == pid && window_info(hwnd).title.starts_with(prefix) {
            unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
            any = true;
        }
    }
    any
}

pub fn bring_to_front(hwnd: isize) {
    unsafe {
        ShowWindow(hwnd as HWND, SW_RESTORE);
        SetForegroundWindow(hwnd as HWND);
    }
}

// ---- monitors / GPU ------------------------------------------------------------------------
pub fn make_dpi_aware() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// The monitor the game's window is on (else the primary monitor).
fn game_monitor(pids: &[u32]) -> HMONITOR {
    unsafe {
        let mut mon: HMONITOR = std::ptr::null_mut();
        if let Some(w) = (!pids.is_empty()).then(|| main_window(pids)).flatten() {
            mon = MonitorFromWindow(w.hwnd as HWND, MONITOR_DEFAULTTONEAREST);
        }
        if mon.is_null() {
            mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        }
        mon
    }
}

/// The monitor's device interface path ("\?\DISPLAY#...#{e6f07b5f-...}"): what OBS's
/// monitor capture wants as `monitor_id` (it ignores the old numeric `monitor`).
pub fn monitor_device_id(pids: &[u32]) -> Option<String> {
    unsafe {
        let mon = game_monitor(pids);
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if mon.is_null() || GetMonitorInfoW(mon, &mut info as *mut _ as *mut MONITORINFO) == 0 {
            return None;
        }
        let mut first = None;
        for i in 0..8 {
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if EnumDisplayDevicesW(info.szDevice.as_ptr(), i, &mut dd, EDD_GET_DEVICE_INTERFACE_NAME) == 0 {
                break;
            }
            let id = String::from_utf16_lossy(&dd.DeviceID[..dd.DeviceID.iter().position(|&c| c == 0).unwrap_or(dd.DeviceID.len())]);
            if dd.StateFlags & DISPLAY_DEVICE_ACTIVE != 0 {
                return Some(id);
            }
            first.get_or_insert(id);
        }
        first
    }
}

/// Pixel size of the monitor the game's window is on (else the primary monitor).
pub fn monitor_size(pids: &[u32]) -> (u32, u32) {
    unsafe {
        let mon = game_monitor(pids);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !mon.is_null() && GetMonitorInfoW(mon, &mut info) != 0 {
            let r = info.rcMonitor;
            return ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
        }
        (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32)
    }
}

/// Work area (screen minus taskbar) of the monitor the game is on, else the primary monitor.
pub fn monitor_work_area(pids: &[u32]) -> RECT {
    unsafe {
        let mut mon: HMONITOR = std::ptr::null_mut();
        if let Some(w) = (!pids.is_empty()).then(|| main_window(pids)).flatten() {
            mon = MonitorFromWindow(w.hwnd as HWND, MONITOR_DEFAULTTONEAREST);
        }
        if mon.is_null() {
            mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut info);
        info.rcWork
    }
}

pub fn gpu_names() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        for i in 0..16 {
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if EnumDisplayDevicesW(std::ptr::null(), i, &mut dd, 0) == 0 {
                break;
            }
            let name = from_wide(&dd.DeviceString);
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

// ---- registry --------------------------------------------------------------------------------
pub fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(root, wide(key).as_ptr(), wide(value).as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(),
                     buf.as_mut_ptr() as *mut c_void, &mut size)
    };
    (rc == ERROR_SUCCESS).then(|| from_wide(&buf))
}

pub fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    let rc = unsafe {
        RegGetValueW(root, wide(key).as_ptr(), wide(value).as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(),
                     &mut v as *mut u32 as *mut c_void, &mut size)
    };
    (rc == ERROR_SUCCESS).then_some(v)
}

pub const HKCU: HKEY = HKEY_CURRENT_USER;
pub const HKLM: HKEY = HKEY_LOCAL_MACHINE;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Start with Windows (HKCU Run key). `command` = None removes it.
pub fn set_run_at_login(name: &str, command: Option<&str>) -> Result<(), String> {
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKCU, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key) != ERROR_SUCCESS {
            return Err("could not open the Run key".into());
        }
        let rc = match command {
            Some(cmd) => {
                let data = wide(cmd);
                RegSetValueExW(key, wide(name).as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32)
            }
            None => {
                let rc = RegDeleteValueW(key, wide(name).as_ptr());
                if rc == ERROR_FILE_NOT_FOUND { ERROR_SUCCESS } else { rc }
            }
        };
        RegCloseKey(key);
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(format!("registry error {rc}")) }
    }
}

// ---- files ------------------------------------------------------------------------------------
/// Move a file to the Recycle Bin.
pub fn recycle(path: &Path) -> Result<(), String> {
    let mut from: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
    from.extend([0, 0]);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 || path.exists() {
        return Err(format!("could not move {} to the Recycle Bin (code {rc})", path.display()));
    }
    Ok(())
}

/// Hold a named mutex for the process lifetime; false if another copy already holds it.
pub fn single_instance(name: &str) -> bool {
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr());
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

pub const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW

/// A Command that never flashes a console window.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(program);
    c.creation_flags(NO_WINDOW);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_spec_escapes() {
        let w = WindowInfo { hwnd: 0, title: "A:B #1".into(), class: "C".into() };
        assert_eq!(obs_window_spec("game.exe", Some(&w)), "A#3AB #221:C:game.exe");
        assert_eq!(obs_window_spec("game.exe", None), "::game.exe");
    }

    #[test]
    fn lists_processes() {
        let snap = running_processes();
        assert!(snap.names().count() > 5);
        let me = std::env::current_exe().unwrap();
        let exe = me.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(snap.contains(&exe));
        assert!(snap.paths(&exe).iter().any(|p| p.ends_with(&exe)));
    }
}

// ---- file picker -------------------------------------------------------------------------------
/// A standard "Open" dialog for one file. `filter` is (label, pattern), e.g. ("OBS Studio", "obs64.exe").
/// Blocks the calling thread until the user closes it.
pub fn pick_file(title: &str, filter: (&str, &str), start_dir: Option<&Path>) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let mut buf = vec![0u16; 1024];
    let filt: Vec<u16> = format!("{} ({})\0{}\0All files\0*.*\0\0", filter.0, filter.1, filter.1).encode_utf16().collect();
    let title = wide(title);
    let dir = start_dir.map(|d| wide(&d.to_string_lossy()));
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = unsafe { GetForegroundWindow() };
    ofn.lpstrFilter = filt.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    if let Some(d) = &dir { ofn.lpstrInitialDir = d.as_ptr(); }
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER;
    (unsafe { GetOpenFileNameW(&mut ofn) } != 0).then(|| from_wide(&buf))
}

/// Is this window the one in front (has the keyboard focus)?
pub fn is_foreground(hwnd: isize) -> bool {
    unsafe { GetForegroundWindow() as isize == hwnd }
}

/// A Yes/No question in a standard Windows message box (owned by the active window).
pub fn confirm(title: &str, text: &str) -> bool {
    unsafe { MessageBoxW(GetForegroundWindow(), wide(text).as_ptr(), wide(title).as_ptr(), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

/// Set a window's title-bar (small) and taskbar (big) icons.
pub fn set_window_icons(hwnd: isize, small: isize, big: isize) {
    unsafe {
        SendMessageW(hwnd as HWND, WM_SETICON, ICON_SMALL as WPARAM, small as LPARAM);
        SendMessageW(hwnd as HWND, WM_SETICON, ICON_BIG as WPARAM, big as LPARAM);
    }
}
