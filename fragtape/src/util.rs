//! Small shared helpers: logging, paths, time, tokens, file writes.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const APP: &str = "Fragtape";

// ---- logging ------------------------------------------------------------------------
struct Log {
    lines: VecDeque<String>,
    file: Option<fs::File>,
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();

fn log_state() -> &'static Mutex<Log> {
    LOG.get_or_init(|| Mutex::new(Log { lines: VecDeque::new(), file: None }))
}

pub fn init_log(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > 2_000_000 {
            let _ = fs::rename(path, path.with_extension("old.log"));
        }
    }
    let file = fs::OpenOptions::new().create(true).append(true).open(path).ok();
    log_state().lock().unwrap().file = file;
}

pub fn log(msg: impl AsRef<str>) {
    let line = format!("{} {}", local_time("%H:%M:%S"), msg.as_ref());
    let mut l = log_state().lock().unwrap();
    if let Some(f) = l.file.as_mut() {
        let _ = writeln!(f, "{line}");
    }
    if cfg!(debug_assertions) {
        eprintln!("{line}");
    }
    l.lines.push_back(line);
    while l.lines.len() > 300 {
        l.lines.pop_front();
    }
}

pub fn recent_log(n: usize) -> Vec<String> {
    let l = log_state().lock().unwrap();
    l.lines.iter().rev().take(n).rev().cloned().collect()
}

#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => { $crate::util::log(format!($($arg)*)) };
}

// ---- time ---------------------------------------------------------------------------
static START: OnceLock<Instant> = OnceLock::new();

/// Seconds since the app started (monotonic).
pub fn mono() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Local time formatted with %Y %m %d %H %M %S.
pub fn local_time(fmt: &str) -> String {
    let (y, mo, d, h, mi, s) = local_parts();
    fmt.replace("%Y", &format!("{y:04}"))
        .replace("%m", &format!("{mo:02}"))
        .replace("%d", &format!("{d:02}"))
        .replace("%H", &format!("{h:02}"))
        .replace("%M", &format!("{mi:02}"))
        .replace("%S", &format!("{s:02}"))
}

#[cfg(windows)]
fn local_parts() -> (u16, u16, u16, u16, u16, u16) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(not(windows))]
fn local_parts() -> (u16, u16, u16, u16, u16, u16) {
    (1970, 1, 1, 0, 0, 0)
}

// ---- tokens -------------------------------------------------------------------------
pub fn token(bytes: usize) -> String {
    use base64::Engine;
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("random");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

// ---- paths --------------------------------------------------------------------------
fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(home)
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// %APPDATA%\Fragtape: library, game list, log.
pub fn data_dir() -> PathBuf {
    env_dir("APPDATA").join(APP)
}

/// %LOCALAPPDATA%\Fragtape: engine, thumbnails, clip metadata, window profile.
pub fn local_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join(APP)
}

pub fn videos_root() -> PathBuf {
    home().join("Videos").join(APP)
}

/// File-name safe version of a title ("Triple Kill: 3/5" -> "Triple Kill 35").
pub fn safe_name(text: &str) -> String {
    let s: String = text.chars().filter(|c| !"<>:\"/\\|?*".contains(*c) && !c.is_control()).collect();
    let s = s.trim().trim_end_matches('.').trim().to_string();
    if s.is_empty() { "highlight".into() } else { s }
}

/// Write via a temp file + rename, so a crash never leaves half a file.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// "C:\\a\\B" -> "c:/a/b" for comparing paths.
pub fn norm_path(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("Triple Kill: 3/5"), "Triple Kill 35");
        assert_eq!(safe_name("  ..."), "highlight");
    }

    #[test]
    fn tokens_are_random() {
        assert_ne!(token(16), token(16));
        assert_eq!(token(16).len(), 22);
    }
}
