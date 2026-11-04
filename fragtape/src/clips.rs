//! The clip library: every video under Videos\Fragtape\<Game>\, plus what Fragtape
//! knows about each (game, title, markers, duration, favorite) in clips.json.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::events::{Highlight, Marker};
use crate::util::{local_time, read_json, safe_name, write_atomic};
use crate::{logf, win};

const VIDEO_EXTS: [&str; 3] = ["mp4", "mov", "mkv"];

pub fn clip_id(rel: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(rel.to_lowercase().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn title_from_name(stem: &str) -> String {
    // "Clip 2026-11-03 21-00-00 - Triple Kill" -> "Triple Kill"; "01 - Ace" -> "Ace"
    stem.split_once(" - ").map(|(_, t)| t.to_string()).unwrap_or_else(|| stem.to_string())
}

fn mtime(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub struct ClipStore {
    pub root: PathBuf,
    meta_path: PathBuf,
    pub thumbs: PathBuf,
    meta: Mutex<Map<String, Value>>,
    index: Mutex<HashMap<String, PathBuf>>,
}

impl ClipStore {
    pub fn new(root: PathBuf, data_dir: &Path) -> Self {
        let meta_path = data_dir.join("clips.json");
        let meta = read_json(&meta_path).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        ClipStore { root, meta_path, thumbs: data_dir.join("thumbs"), meta: Mutex::new(meta), index: Mutex::default() }
    }

    fn save(&self, meta: &Map<String, Value>) {
        let _ = write_atomic(&self.meta_path, &serde_json::to_vec(meta).unwrap());
    }

    fn rel(&self, p: &Path) -> Option<String> {
        let root = fs::canonicalize(&self.root).ok()?;
        let full = fs::canonicalize(p).ok()?;
        Some(full.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))
    }

    /// Record what we know about a finished video.
    pub fn add(&self, path: &Path, info: Value) {
        let Some(rel) = self.rel(path) else { return };
        let mut meta = self.meta.lock().unwrap();
        let entry = meta.entry(rel).or_insert(json!({}));
        for (k, v) in info.as_object().cloned().unwrap_or_default() {
            if !v.is_null() {
                entry[k] = v;
            }
        }
        self.save(&meta);
    }

    /// All videos, newest first. `live` = path being recorded right now.
    pub fn list(&self, live: Option<&str>) -> Vec<Value> {
        let live = live.map(crate::util::norm_path);
        let meta = self.meta.lock().unwrap().clone();
        let mut out = vec![];
        let mut index = HashMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                let Ok(m) = e.metadata() else { continue };
                if m.is_dir() {
                    stack.push(p);
                    continue;
                }
                let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
                if !VIDEO_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                let rel = p.strip_prefix(&self.root).unwrap().to_string_lossy().replace('\\', "/");
                let id = clip_id(&rel);
                let stem = p.file_stem().unwrap().to_string_lossy().to_string();
                let info = meta.get(&rel).cloned().unwrap_or(json!({}));
                let parts: Vec<&str> = rel.split('/').collect();
                let is_live = live.as_deref() == Some(crate::util::norm_path(&p.to_string_lossy()).as_str());
                let kind = info["kind"].as_str().map(String::from).unwrap_or_else(|| {
                    if stem.starts_with("Clip") || stem.starts_with("Replay") || rel.contains("Highlights") { "clip".into() } else { "session".into() }
                });
                // A duration measured while the file was still growing doesn't count.
                let duration = if info["dur_mtime"].as_i64() == Some(mtime(&m)) && !is_live { info["duration"].clone() } else { Value::Null };
                let markers: Vec<Value> = info["markers"].as_array().into_iter().flatten()
                    .map(|x| json!({"offset": x["offset"], "kind": x["kind"], "label": x["label"], "detail": x["detail"]})).collect();
                let highlights: Vec<Value> = info["highlights"].as_array().into_iter().flatten()
                    .map(|h| json!({"start": h["start"], "end": h["end"], "title": h["title"]})).collect();
                out.push(json!({
                    "id": id, "rel": rel, "name": p.file_name().unwrap().to_string_lossy(),
                    "game": info["game"].as_str().map(String::from).unwrap_or_else(|| if parts.len() > 1 { parts[0].into() } else { "Other".into() }),
                    "kind": kind, "title": info["title"].as_str().map(String::from).unwrap_or_else(|| title_from_name(&stem)),
                    "size": m.len(), "mtime": mtime(&m), "duration": duration, "live": is_live,
                    "markers": markers, "highlights": highlights,
                    "thumb": self.thumbs.join(format!("{id}.jpg")).exists(), "favorite": info["favorite"] == true,
                }));
                index.insert(id, p);
            }
        }
        *self.index.lock().unwrap() = index;
        out.sort_by_key(|c| -c["mtime"].as_i64().unwrap_or(0));
        out
    }

    pub fn path_for(&self, id: &str) -> Option<PathBuf> {
        let hit = self.index.lock().unwrap().get(id).cloned();
        let p = hit.or_else(|| {
            self.list(None);
            self.index.lock().unwrap().get(id).cloned()
        })?;
        p.exists().then_some(p)
    }

    pub fn update(&self, id: &str, fields: &Value) -> Result<(), String> {
        let p = self.path_for(id).ok_or("clip not found")?;
        let rel = p.strip_prefix(&self.root).unwrap().to_string_lossy().replace('\\', "/");
        let mut meta = self.meta.lock().unwrap();
        let entry = meta.entry(rel).or_insert(json!({}));
        if let Some(t) = fields["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
            entry["title"] = json!(t.chars().take(120).collect::<String>());
        }
        if let Some(f) = fields["favorite"].as_bool() {
            entry["favorite"] = json!(f);
        }
        if let Some(d) = fields["duration"].as_f64().filter(|d| d.is_finite() && *d >= 0.0) {
            entry["duration"] = json!(d);
            entry["dur_mtime"] = json!(fs::metadata(&p).map(|m| mtime(&m)).unwrap_or(0));
        }
        self.save(&meta);
        Ok(())
    }

    pub fn save_thumb(&self, id: &str, jpeg: &[u8]) -> Result<(), String> {
        if self.path_for(id).is_none() || !jpeg.starts_with(&[0xff, 0xd8]) || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("not a jpeg".into());
        }
        fs::create_dir_all(&self.thumbs).map_err(|e| e.to_string())?;
        fs::write(self.thumbs.join(format!("{id}.jpg")), jpeg).map_err(|e| e.to_string())
    }

    pub fn thumb_path(&self, id: &str) -> Option<PathBuf> {
        if !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let p = self.thumbs.join(format!("{id}.jpg"));
        p.exists().then_some(p)
    }

    /// Move to the Recycle Bin (a mis-click can be undone).
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let p = self.path_for(id).ok_or("clip not found")?;
        win::recycle(&p)?;
        let rel = p.strip_prefix(&self.root).unwrap().to_string_lossy().replace('\\', "/");
        let mut meta = self.meta.lock().unwrap();
        meta.remove(&rel);
        self.save(&meta);
        let _ = fs::remove_file(self.thumbs.join(format!("{id}.jpg")));
        Ok(())
    }

    /// Save [start, end] as a new clip next to the original (stream copy, no re-encode).
    pub fn trim(&self, id: &str, start: f64, end: f64, ffmpeg: Option<&Path>, title: &str) -> Result<String, String> {
        let src = self.path_for(id).ok_or("clip not found")?;
        let ffmpeg = ffmpeg.ok_or("saving clips needs ffmpeg (Settings → Clip editor → Install)")?;
        if end - start < 1.0 {
            return Err("the clip must be at least 1 second long".into());
        }
        let rel = src.strip_prefix(&self.root).unwrap().to_string_lossy().replace('\\', "/");
        let info = self.meta.lock().unwrap().get(&rel).cloned().unwrap_or(json!({}));
        let name = if title.trim().is_empty() { "Clip".to_string() } else { title.trim().to_string() };
        let dst = src.with_file_name(format!("Clip {} - {}.mp4", local_time("%Y-%m-%d %H-%M-%S"), safe_name(&name)));
        cut(ffmpeg, &src, start, end - start, &dst)?;
        let markers: Vec<Value> = info["markers"].as_array().into_iter().flatten()
            .filter(|m| m["offset"].as_f64().is_some_and(|o| o >= start && o <= end))
            .map(|m| { let mut m = m.clone(); m["offset"] = json!(((m["offset"].as_f64().unwrap() - start) * 100.0).round() / 100.0); m })
            .collect();
        self.add(&dst, json!({"kind": "clip", "game": info["game"], "title": name, "markers": markers}));
        Ok(dst.file_name().unwrap().to_string_lossy().to_string())
    }
}

/// ffmpeg stream copy of [start, start+len] into `dst`.
pub fn cut(ffmpeg: &Path, src: &Path, start: f64, len: f64, dst: &Path) -> Result<(), String> {
    let out = win::command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &format!("{start:.3}"), "-i"]).arg(src)
        .args(["-t", &format!("{len:.3}"), "-map", "0:v:0", "-map", "0:a?", "-c", "copy", "-avoid_negative_ts", "make_zero",
               "-movflags", "+faststart"]).arg(dst)
        .output().map_err(|e| e.to_string())?;
    if !out.status.success() || !dst.exists() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.trim().chars().rev().take(300).collect::<String>().chars().rev().collect());
    }
    Ok(())
}

/// Where ffmpeg is (cached for 15 s: the window asks every 2 s).
pub fn find_ffmpeg() -> Option<PathBuf> {
    static CACHE: Mutex<Option<(Instant, Option<PathBuf>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    if let Some((at, found)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(15) {
            return found.clone();
        }
    }
    let found = search_ffmpeg();
    *cache = Some((Instant::now(), found.clone()));
    found
}

fn search_ffmpeg() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join("ffmpeg.exe")).collect()).unwrap_or_default();
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        c.push(PathBuf::from(&l).join("Microsoft").join("WinGet").join("Links").join("ffmpeg.exe"));
        // winget installs here; its Links folder isn't always on PATH for running apps
        let pkgs = PathBuf::from(&l).join("Microsoft").join("WinGet").join("Packages");
        for e in fs::read_dir(pkgs).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with("Gyan.FFmpeg") {
                for sub in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    c.push(sub.path().join("bin").join("ffmpeg.exe"));
                }
            }
        }
    }
    c.push(crate::util::home().join("scoop").join("shims").join("ffmpeg.exe"));
    c.push(PathBuf::from(r"C:\ffmpeg\bin\ffmpeg.exe"));
    c.push(PathBuf::from(r"C:\ProgramData\chocolatey\bin\ffmpeg.exe"));
    c.into_iter().find(|p| p.is_file())
}

/// Wait until OBS has finished writing the file (size stable for 2 s).
pub fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    let mut stable = 0;
    while Instant::now() < deadline {
        if let Ok(m) = fs::metadata(path) {
            let cur = Some(m.len());
            stable = if cur == last && m.len() > 0 { stable + 1 } else { 0 };
            if stable >= 2 {
                return true;
            }
            last = cur;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    path.exists()
}

/// After a recording: store its markers, and cut highlight clips if ffmpeg is there.
pub fn finish_session(store: &ClipStore, path: &Path, game: Option<String>, markers: &[Marker], highlights: &[Highlight]) {
    if !wait_for_file(path, Duration::from_secs(60)) {
        return logf!("recording file not found: {}", path.display());
    }
    store.add(path, json!({"kind": "session", "game": game, "markers": markers, "highlights": highlights}));
    if highlights.is_empty() {
        return;
    }
    let Some(ffmpeg) = find_ffmpeg() else {
        return logf!("{} highlights marked; install ffmpeg to also export them as separate clips", highlights.len());
    };
    let dir = path.parent().unwrap().join("Highlights");
    let _ = fs::create_dir_all(&dir);
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let mut n = 0;
    for (i, h) in highlights.iter().enumerate() {
        let dst = dir.join(format!("{stem} {:02} - {}.mp4", i + 1, safe_name(&h.title)));
        match cut(&ffmpeg, path, h.start, h.end - h.start, &dst) {
            Ok(()) => {
                n += 1;
                let ms: Vec<Value> = h.markers.iter().map(|m| json!({"offset": ((m.offset - h.start) * 100.0).round() / 100.0, "label": m.label, "detail": m.detail})).collect();
                store.add(&dst, json!({"kind": "clip", "game": game, "title": h.title, "markers": ms}));
            }
            Err(e) => logf!("[clips] ffmpeg failed: {e}"),
        }
    }
    logf!("exported {n} highlight clips to {}", dir.display());
}

/// After a replay save: "Clip 2026-11-04 10-00-00.mp4" -> "... - Triple Kill.mp4", then store it.
pub fn finish_clip(store: &ClipStore, path: &Path, game: Option<String>, title: &str) -> PathBuf {
    let dst = path.with_file_name(format!("{} - {}.{}", path.file_stem().unwrap().to_string_lossy(), safe_name(title),
                                          path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or("mp4".into())));
    let mut final_path = path.to_path_buf();
    for _ in 0..20 {
        match fs::rename(path, &dst) {
            Ok(()) => { final_path = dst; break; }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => std::thread::sleep(Duration::from_millis(500)), // OBS may still be writing it
        }
    }
    store.add(&final_path, json!({"kind": "clip", "game": game, "title": title}));
    logf!("[replay] saved {}", final_path.file_name().unwrap().to_string_lossy());
    final_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::tempdir;

    fn store() -> (tempdir::Dir, ClipStore, PathBuf, PathBuf) {
        let d = tempdir::Dir::new();
        let root = d.0.join("videos");
        fs::create_dir_all(root.join("Valorant")).unwrap();
        let rec = root.join("Valorant").join("2026-11-04 10-00-00.mp4");
        fs::write(&rec, vec![0u8; 100]).unwrap();
        let clip = root.join("Valorant").join("Clip 2026-11-04 10-05-00 - Triple Kill.mp4");
        fs::write(&clip, vec![0u8; 50]).unwrap();
        fs::write(root.join("Valorant").join("notes.txt"), "x").unwrap();
        let s = ClipStore::new(root, &d.0.join("data"));
        (d, s, rec, clip)
    }

    #[test]
    fn lists_with_guesses() {
        let (_d, s, rec, _) = store();
        let l = s.list(None);
        assert_eq!(l.len(), 2);
        let clip = l.iter().find(|c| c["kind"] == "clip").unwrap();
        assert_eq!((clip["title"].clone(), clip["game"].clone()), (json!("Triple Kill"), json!("Valorant")));
        s.add(&rec, json!({"kind": "session", "game": "VALORANT", "markers": [{"offset": 3.0, "label": "Kill", "detail": ""}]}));
        let l = s.list(Some(&rec.to_string_lossy()));
        let r = l.iter().find(|c| c["kind"] == "session").unwrap();
        assert_eq!((r["game"].clone(), r["live"].clone(), r["markers"].as_array().unwrap().len()), (json!("VALORANT"), json!(true), 1));
    }

    #[test]
    fn duration_resets_when_file_changes_and_updates_are_filtered() {
        let (_d, s, rec, _) = store();
        let id = s.list(None).into_iter().find(|c| c["kind"] == "session").unwrap()["id"].as_str().unwrap().to_string();
        s.update(&id, &json!({"duration": 2.0, "title": "Ace", "path": "C:/evil"})).unwrap();
        let get = || s.list(None).into_iter().find(|c| c["id"] == json!(id)).unwrap();
        assert_eq!((get()["duration"].clone(), get()["title"].clone()), (json!(2.0), json!("Ace")));
        assert!(!serde_json::to_string(&*s.meta.lock().unwrap()).unwrap().contains("evil"));
        let f = fs::OpenOptions::new().write(true).open(&rec).unwrap();
        f.set_modified(std::time::SystemTime::now() + Duration::from_secs(60)).unwrap();
        assert!(get()["duration"].is_null());
    }

    #[test]
    fn thumbs_must_be_jpeg_and_trim_needs_ffmpeg() {
        let (_d, s, _, _) = store();
        let id = s.list(None)[0]["id"].as_str().unwrap().to_string();
        assert!(s.save_thumb(&id, b"<svg>").is_err());
        s.save_thumb(&id, &[0xff, 0xd8, 0xff, 0xe0]).unwrap();
        assert!(s.thumb_path(&id).is_some());
        assert!(s.thumb_path("../x").is_none());
        assert!(s.trim(&id, 0.0, 10.0, None, "x").is_err());
    }

    #[test]
    fn outside_root_is_ignored() {
        let (d, s, _, _) = store();
        let other = d.0.join("elsewhere.mp4");
        fs::write(&other, b"x").unwrap();
        s.add(&other, json!({"kind": "clip"}));
        assert!(s.meta.lock().unwrap().is_empty());
    }
}
