//! Thumbnails and game icons for the native window: loaded from disk once and cached,
//! and made in the background (thumbnails with Windows' own video engine).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slint::Image;

use crate::app::Shared;
use crate::video::{self, Video};

/// Images already decoded, by file (reloaded when the file changes).
#[derive(Default)]
pub struct Cache {
    map: HashMap<PathBuf, (u64, Image)>,
}

fn mtime(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

impl Cache {
    pub fn get(&mut self, p: &Path) -> Image {
        let Some(m) = mtime(p) else { return Image::default() };
        if let Some((mm, img)) = self.map.get(p) {
            if *mm == m {
                return img.clone();
            }
        }
        let img = Image::load_from_path(p).unwrap_or_default();
        self.map.insert(p.to_path_buf(), (m, img.clone()));
        img
    }
}

pub fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgba);
        if let Ok(mut wr) = e.write_header() {
            let _ = wr.write_image_data(rgba);
        }
    }
    out
}

/// Background work for the window; `dirty` turns on when something new is ready to show.
pub struct Workers {
    thumbs: Sender<(String, PathBuf)>,
    icons: Sender<(String, Value, Option<String>)>,
    asked: Mutex<HashSet<String>>,
    pub dirty: Arc<AtomicBool>,
    pub icon_dir: PathBuf,
}

impl Workers {
    pub fn start(s: Arc<Shared>) -> Workers {
        let dirty = Arc::new(AtomicBool::new(false));
        let icon_dir = crate::util::local_dir().join("icons").join("ui");
        let _ = std::fs::create_dir_all(&icon_dir);
        // thumbnails + durations
        let (tx, rx) = channel::<(String, PathBuf)>();
        let (s2, d2) = (s.clone(), dirty.clone());
        std::thread::Builder::new().name("thumbs".into()).spawn(move || {
            let mut engine: Option<Video> = None;
            loop {
                // let the video engine go when there is nothing to do for a while
                let (id, path) = match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(job) => job,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => { engine = None; continue; }
                    Err(_) => break,
                };
                if engine.is_none() {
                    engine = Video::new(true).map_err(|e| crate::logf!("[ui] thumbnail engine: {e}")).ok();
                }
                let Some(v) = engine.as_mut() else { continue };
                match thumbnail(v, &path) {
                    Some((png, dur)) => {
                        let _ = s2.clips.save_thumb_png(&id, &png);
                        if dur > 0.0 {
                            let _ = s2.clips.update(&id, &json!({"duration": (dur * 10.0).round() / 10.0}));
                        }
                        d2.store(true, Ordering::Relaxed);
                    }
                    None => crate::logf!("[ui] no thumbnail for {}", path.display()),
                }
            }
        }).ok();
        // game icons (from the exe, Steam or Discord; can download)
        let (itx, irx) = channel::<(String, Value, Option<String>)>();
        let (s3, d3, dir) = (s.clone(), dirty.clone(), icon_dir.clone());
        std::thread::Builder::new().name("icons".into()).spawn(move || {
            for (key, game, url) in irx {
                if let Some(bytes) = s3.icons.for_game(&game, url) {
                    let ext = match crate::icons::content_type(&bytes) { "image/png" => "png", "image/jpeg" => "jpg", "image/x-icon" => "ico", _ => "png" };
                    let _ = std::fs::write(dir.join(format!("{key}.{ext}")), bytes);
                    d3.store(true, Ordering::Relaxed);
                }
            }
        }).ok();
        Workers { thumbs: tx, icons: itx, asked: Mutex::default(), dirty, icon_dir }
    }

    pub fn want_thumb(&self, id: &str, path: PathBuf) {
        if self.asked.lock().unwrap().insert(format!("t:{id}")) {
            let _ = self.thumbs.send((id.to_string(), path));
        }
    }

    /// The icon file for a game if we have it; otherwise ask for it (once).
    pub fn icon(&self, name: &str, game: Option<&Value>, discord: Option<String>) -> Option<PathBuf> {
        let key = crate::icons::key(name);
        for ext in ["png", "jpg", "ico"] {
            let p = self.icon_dir.join(format!("{key}.{ext}"));
            if p.exists() {
                return Some(p);
            }
        }
        if let Some(g) = game {
            if self.asked.lock().unwrap().insert(format!("i:{key}")) {
                let _ = self.icons.send((key, g.clone(), discord));
            }
        }
        None
    }
}

/// Open, wait for the video, show a frame about a third in (at most 12 s), return it as PNG.
fn thumbnail(v: &mut Video, path: &Path) -> Option<(Vec<u8>, f64)> {
    v.open(path).ok()?;
    let wait = |v: &Video, bit: u32, secs: f64| {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            if v.flags.take_one(bit) { return true; }
            if v.flags.take_one(video::EV_ERROR) { return false; }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    };
    let frame_ready = |v: &Video| {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < 3.0 {
            if v.new_frame() { return true; }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    };
    let _ = v.flags.take();
    if !wait(v, video::EV_LOADED, 8.0) {
        return None;
    }
    let dur = v.duration();
    v.play();
    frame_ready(v);
    v.pause();
    let _ = v.flags.take();
    v.seek((dur * 0.3).min(12.0));
    wait(v, video::EV_SEEKED, 4.0);
    frame_ready(v);
    let mut buf = Vec::new();
    v.frame(384, 216, &mut buf).ok()?;
    Some((encode_png(384, 216, &buf), dur))
}
