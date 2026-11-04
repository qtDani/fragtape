//! Game icons, taken from the PC itself whenever possible:
//!   1. the game's own .exe (its embedded icon, 64 px)
//!   2. Steam's local library cache (appcache\librarycache\<appid>\<hash>.jpg)
//!   3. Discord's icon, downloaded once and kept (only for games that aren't installed)
//! Every icon is cached in %LOCALAPPDATA%\Fragtape\icons, so the window only ever loads
//! icons from 127.0.0.1.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::{http, importers, win};

pub fn key(name: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(name.to_lowercase().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub fn content_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if data.starts_with(&[0xff, 0xd8]) {
        "image/jpeg"
    } else {
        "image/x-icon"
    }
}

pub struct Icons {
    dir: PathBuf,
    missing: Mutex<HashSet<String>>, // looked up already, nothing found (this run)
    lock: Mutex<()>,
}

impl Icons {
    pub fn new(dir: PathBuf) -> Self {
        Icons { dir, missing: Mutex::default(), lock: Mutex::new(()) }
    }

    fn cached(&self, k: &str) -> Option<Vec<u8>> {
        fs::read(self.dir.join(format!("{k}.img"))).ok()
    }

    fn store(&self, k: &str, data: &[u8]) {
        let _ = fs::create_dir_all(&self.dir);
        let _ = fs::write(self.dir.join(format!("{k}.img")), data);
    }

    /// Icon for a library game (its JSON entry).
    pub fn for_game(&self, game: &Value, discord_url: Option<String>) -> Option<Vec<u8>> {
        let name = game["name"].as_str()?;
        let k = key(name);
        if let Some(d) = self.cached(&k) {
            return Some(d);
        }
        if self.missing.lock().unwrap().contains(&k) {
            return None;
        }
        let _guard = self.lock.lock().unwrap(); // one extraction at a time, at low priority
        win::lower_thread_priority();
        let found = exe_paths(game).iter().find_map(|p| exe_icon(p))
            .or_else(|| game["steam_appid"].as_str().and_then(steam_icon))
            .or_else(|| discord_url.and_then(|u| http::get(&u, 8000, false).ok()));
        match &found {
            Some(d) => self.store(&k, d),
            None => { self.missing.lock().unwrap().insert(k); }
        }
        found
    }

    /// Icon of a game in the search results (not in the library yet): Discord's, cached.
    pub fn for_db(&self, id: &str, url: &str) -> Option<Vec<u8>> {
        let k = format!("db-{}", id.chars().filter(|c| c.is_ascii_digit()).collect::<String>());
        if let Some(d) = self.cached(&k) {
            return Some(d);
        }
        let d = http::get(url, 8000, false).ok()?;
        self.store(&k, &d);
        Some(d)
    }
}

/// Full paths of the game's .exe files that exist on this PC.
fn exe_paths(game: &Value) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Some(p) = game["exe_path"].as_str() {
        out.push(PathBuf::from(p));
    }
    // Steam games store "folder/sub/game.exe" relative to steamapps\common.
    let rel: Vec<&str> = game["exes"].as_array().into_iter().flatten().filter_map(|e| e.as_str()).filter(|e| e.contains('/')).collect();
    if !rel.is_empty() && game["steam_appid"].is_string() {
        for lib in importers::steam_libraries() {
            for r in &rel {
                out.push(lib.join("steamapps").join("common").join(r.replace('/', "\\")));
            }
        }
    }
    out.into_iter().filter(|p| p.is_file()).collect()
}

/// Steam's own small icon for an app: the "<40 hex chars>.jpg" in librarycache\<appid>.
fn steam_icon(appid: &str) -> Option<Vec<u8>> {
    for lib in importers::steam_libraries() {
        let dir = lib.join("appcache").join("librarycache").join(appid);
        let old_style = lib.join("appcache").join("librarycache").join(format!("{appid}_icon.jpg"));
        if let Ok(d) = fs::read(&old_style) {
            return Some(d);
        }
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.len() == 44 && n.ends_with(".jpg") && n[..40].chars().all(|c| c.is_ascii_hexdigit()) {
                if let Ok(d) = fs::read(e.path()) {
                    return Some(d);
                }
            }
        }
    }
    None
}

/// The icon embedded in an .exe, as a 64x64 32-bit .ico.
pub fn exe_icon(path: &Path) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    const SIZE: i32 = 64;
    unsafe {
        let mut hicon: HICON = std::ptr::null_mut();
        let mut id = 0u32;
        let n = PrivateExtractIconsW(win::wide(&path.to_string_lossy()).as_ptr(), 0, SIZE, SIZE, &mut hicon, &mut id, 1, 0);
        if n == 0 || n == u32::MAX || hicon.is_null() {
            return None;
        }
        let mut info: ICONINFO = std::mem::zeroed();
        let ok = GetIconInfo(hicon, &mut info) != 0;
        let result = if ok && !info.hbmColor.is_null() {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = SIZE;
            bmi.bmiHeader.biHeight = SIZE; // bottom-up, as .ico wants
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
            let lines = GetDIBits(dc, info.hbmColor, 0, SIZE as u32, px.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
            DeleteDC(dc);
            if lines == SIZE {
                if px.chunks(4).all(|p| p[3] == 0) {
                    // Old icons without alpha: opaque, except pure black corners are kept as they are.
                    for p in px.chunks_mut(4) { p[3] = 255; }
                }
                Some(ico(SIZE as u32, px))
            } else {
                None
            }
        } else {
            None
        };
        if !info.hbmColor.is_null() { DeleteObject(info.hbmColor); }
        if !info.hbmMask.is_null() { DeleteObject(info.hbmMask); }
        DestroyIcon(hicon);
        result
    }
}

/// Wrap bottom-up BGRA pixels in a single-image .ico.
pub fn ico(size: u32, bgra: Vec<u8>) -> Vec<u8> {
    let mask = vec![0u8; (size.div_ceil(32) * 4 * size) as usize];
    let mut bmp = Vec::new();
    for v in [40u32, size, size * 2] { bmp.extend(v.to_le_bytes()); }
    bmp.extend(1u16.to_le_bytes());
    bmp.extend(32u16.to_le_bytes());
    for v in [0u32, (bgra.len() + mask.len()) as u32, 0, 0, 0, 0] { bmp.extend(v.to_le_bytes()); }
    bmp.extend(bgra);
    bmp.extend(mask);
    let mut out = vec![0, 0, 1, 0, 1, 0, size as u8, size as u8, 0, 0, 1, 0, 32, 0];
    out.extend((bmp.len() as u32).to_le_bytes());
    out.extend(22u32.to_le_bytes());
    out.extend(bmp);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_an_exe_icon() {
        // explorer.exe always has an icon
        let d = exe_icon(Path::new(r"C:\Windows\explorer.exe")).expect("icon");
        assert_eq!(&d[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!(d[6], 64);
        assert_eq!(content_type(&d), "image/x-icon");
        assert!(exe_icon(Path::new(r"C:\nope.exe")).is_none());
    }

    #[test]
    fn keys_ignore_case() {
        assert_eq!(key("Apex Legends"), key("apex legends"));
        assert_eq!(content_type(&[0xff, 0xd8, 0]), "image/jpeg");
    }
}
