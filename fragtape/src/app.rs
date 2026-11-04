//! Wiring: shared state, the main loop, the window, first-run migration.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::clips::{self, ClipStore};
use crate::controller::{Asker, Controller, Media};
use crate::engine::{Engine, EngineEvent, EngineObs, Obs, Options, Target};
use crate::events::has_kill_detection;
use crate::gamedb::GameDb;
use crate::library::{Library, FULL, HIGHLIGHTS};
use crate::quality::{self, Vendor};
use crate::util::{self, data_dir, local_dir, mono, videos_root};
use crate::{importers, logf, providers, server, tray, win};

pub const PORT: u16 = 3133;
const IDLE_STOP_AFTER: f64 = 15.0; // seconds without a game/recording before the hidden OBS closes

#[derive(Clone, Debug)]
pub enum Cmd {
    Answer(String, Option<String>),
    Mark,
    PrefChanged(String),
    Stop,
    Show,
    Quit,
}

/// State shared between the main loop, the web server and the tray.
pub struct Shared {
    pub library: Arc<Library>,
    pub gamedb: Arc<GameDb>,
    pub clips: Arc<ClipStore>,
    pub engine: Arc<Engine>,
    pub status: Mutex<Value>,
    pub active_path: Mutex<Option<String>>,
    pub scanning: Arc<AtomicBool>,
    pub installing_ffmpeg: Arc<AtomicBool>,
    pub installing_obs: Arc<AtomicBool>,
    /// The last OBS install/locate problem, shown in the window.
    pub obs_note: Arc<Mutex<String>>,
    tx: Mutex<Sender<Cmd>>,
    pub port: u16,
    pub token: String,
    pub vendor: Vendor,
    pub icons: crate::icons::Icons,
}

impl Shared {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.lock().unwrap().send(c);
    }
}

// ---- options for the hidden OBS --------------------------------------------------------------
pub fn build_options(s: &Shared, t: &Target) -> Options {
    let p = s.library.prefs();
    let int = |k: &str| p[k].as_i64().unwrap_or(0);
    let st = |k: &str| p[k].as_str().unwrap_or("").to_string();
    let custom = p["quality"] == "custom";
    let available = s.engine.encoders.lock().unwrap().clone();
    let encoder = quality::pick_encoder(&p, s.vendor, available.as_deref());
    let ten_bit = custom && p["adv_color"] == "p010" && !encoder.contains("h264") && encoder != "obs_x264";
    let base = win::monitor_size(&t.pids);
    let game = if t.game.is_empty() { "Other".to_string() } else { t.game.clone() };
    Options {
        rec_dir: videos_root().join(util::safe_name(&game)),
        encoder_settings: quality::encoder_settings(&encoder, &p, ten_bit),
        encoder,
        fps: if int("adv_fps") > 0 { int("adv_fps") } else { int("fps") },
        base,
        out: quality::output_size(base, int("adv_resolution")),
        scale_filter: st("adv_scale_filter"),
        color: if ten_bit { "P010".into() } else { "NV12".into() },
        capture: st("capture"),
        audio: st("audio"),
        mic_mono: p["mic_mono"] == true,
        replay_seconds: int("clip_length"),
        replay_mb: int("adv_replay_mb"),
        window_spec: t.spec.clone().unwrap_or_default(),
        audio_bitrate: int("adv_audio_bitrate"),
        sample_rate: int("adv_sample_rate"),
        separate_tracks: p["adv_tracks"] == "separate",
        format: st("adv_format"),
        priority: st("adv_priority"),
    }
}

// ---- state for the window -------------------------------------------------------------------------
/// Discord's icon URL for a game: only used as a last resort, downloaded once by Fragtape itself.
pub fn discord_icon_url(s: &Shared, g: &Value) -> Option<String> {
    if let (Some(id), Some(icon)) = (g["discord_id"].as_str(), g["icon"].as_str()) {
        return Some(format!("https://cdn.discordapp.com/app-icons/{id}/{icon}.png?size=64"));
    }
    let name = g["name"].as_str()?;
    s.gamedb.by_name(name)
        .or_else(|| g["exes"].as_array()?.iter().find_map(|e| s.gamedb.for_exe(e.as_str()?)))
        .and_then(|d| d.icon_url())
}

pub fn state(s: &Shared) -> Value {
    let games: Vec<Value> = s.library.games().iter().map(|g| {
        let name = g["name"].as_str().unwrap_or("");
        json!({"name": name, "mode": g["mode"], "sessions": g["sessions"], "playtime": g["playtime"],
               "last_played": g["last_played"], "source": g["source"], "exes": g["exes"],
               "icon": format!("/icon/{}?t={}", crate::icons::key(name), s.token),
               "kills": has_kill_detection(name), "installed": g["installed"], "added": g["added"]})
    }).collect();
    let next = build_options(s, &Target::default());
    let obs = s.engine.obs_dir();
    let available = s.engine.encoders.lock().unwrap().clone().unwrap_or_default();
    json!({
        "status": *s.status.lock().unwrap(),
        "default_mode": s.library.default_mode(),
        "prefs": s.library.prefs(),
        "games": games,
        "db": {"ready": s.gamedb.ready(), "count": s.gamedb.count()},
        "scanning": s.scanning.load(Ordering::Relaxed),
        "page_rev": server::PAGE.len(),
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "obs_found": obs.is_some(),
            "obs": {"path": obs, "custom": s.engine.custom_obs.lock().unwrap().clone(),
                    "installing": s.installing_obs.load(Ordering::Relaxed), "note": s.obs_note.lock().unwrap().clone()},
            "engine": s.engine.running(),
            "ffmpeg": clips::find_ffmpeg().is_some(),
            "installing_ffmpeg": s.installing_ffmpeg.load(Ordering::Relaxed),
            "folder": videos_root(),
            "vendor": format!("{:?}", s.vendor),
            "encoders": available.iter().map(|e| json!({"id": e, "name": quality::encoder_name(e)})).collect::<Vec<_>>(),
            "next": {"encoder": quality::encoder_name(&next.encoder), "base": format!("{}x{}", next.base.0, next.base.1),
                     "out": format!("{}x{}", next.out.0, next.out.1), "fps": next.fps},
            "log": util::recent_log(40),
        },
    })
}

// ---- background jobs ------------------------------------------------------------------------------
/// Look for games on this PC (idle priority; skipped if launcher files didn't change).
pub fn scan_async(s: &Shared, force: bool) {
    if !s.gamedb.ready() || s.scanning.swap(true, Ordering::Relaxed) {
        return;
    }
    let (lib, db, flag) = (s.library.clone(), s.gamedb.clone(), s.scanning.clone());
    std::thread::Builder::new().name("scan".into()).spawn(move || {
        win::lower_thread_priority();
        let sig = importers::signature();
        if !force && lib.get_raw("scan_signature").as_str() == Some(sig.as_str()) {
            logf!("[scan] nothing changed since the last scan");
        } else {
            let t0 = std::time::Instant::now();
            let found = importers::scan(&db);
            let added = lib.merge_imported(&found);
            lib.set_raw("scan_signature", json!(sig));
            logf!("[scan] found {} games on this PC, {added} new ({} ms)", found.len(), t0.elapsed().as_millis());
        }
        flag.store(false, Ordering::Relaxed);
    }).ok();
}

pub fn install_ffmpeg(s: &Shared) {
    if s.installing_ffmpeg.swap(true, Ordering::Relaxed) {
        return;
    }
    let flag = s.installing_ffmpeg.clone();
    std::thread::spawn(move || {
        logf!("installing ffmpeg (winget install Gyan.FFmpeg)…");
        let r = win::command("winget").args(["install", "--id", "Gyan.FFmpeg", "-e", "--silent",
                                             "--accept-package-agreements", "--accept-source-agreements"]).output();
        match r {
            Ok(o) if o.status.success() => logf!("ffmpeg installed"),
            Ok(o) => logf!("ffmpeg install failed: {}", String::from_utf8_lossy(&o.stdout).chars().rev().take(200).collect::<String>().chars().rev().collect::<String>()),
            Err(e) => logf!("ffmpeg install failed: {e}"),
        }
        flag.store(false, Ordering::Relaxed);
    });
}

pub const OBS_DOWNLOAD: &str = "https://obsproject.com/download";

pub fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
}

/// Install OBS Studio with winget (Windows asks for permission). Without winget, open the download page.
pub fn install_obs(s: &Shared) {
    if s.installing_obs.swap(true, Ordering::Relaxed) {
        return;
    }
    let (flag, note) = (s.installing_obs.clone(), s.obs_note.clone());
    *note.lock().unwrap() = String::new();
    std::thread::spawn(move || {
        logf!("installing OBS Studio (winget install OBSProject.OBSStudio)…");
        let r = win::command("winget").args(["install", "--id", "OBSProject.OBSStudio", "-e", "--silent",
                                             "--accept-package-agreements", "--accept-source-agreements"]).output();
        let msg = match r {
            Ok(_) if crate::engine::find_obs().is_some() => { logf!("OBS Studio installed"); String::new() }
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout);
                let tail: String = out.trim().lines().last().unwrap_or("").chars().take(160).collect();
                logf!("OBS install failed: {tail}");
                format!("The install didn't finish ({}). You can download it from obsproject.com instead.",
                        if tail.is_empty() { "cancelled?" } else { tail.as_str() })
            }
            Err(_) => {
                logf!("winget isn't available; opening the OBS download page");
                open_url(OBS_DOWNLOAD);
                "Windows' installer (winget) isn't available here, so the download page was opened.".into()
            }
        };
        *note.lock().unwrap() = msg;
        flag.store(false, Ordering::Relaxed);
    });
}

/// Use OBS from a folder the user chose (None = auto-detect). Returns the OBS root in use.
pub fn set_obs_path(s: &Shared, picked: Option<&str>) -> Result<Option<PathBuf>, String> {
    let root = match picked.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Some(crate::engine::obs_root(Path::new(p))?),
        None => None,
    };
    s.library.set_raw("obs_path", root.as_ref().map(|p| json!(p)).unwrap_or(Value::Null));
    *s.engine.custom_obs.lock().unwrap() = root;
    s.obs_note.lock().unwrap().clear();
    let using = s.engine.obs_dir();
    logf!("OBS: {}", using.as_ref().map(|p| p.display().to_string()).unwrap_or("not found".into()));
    Ok(using)
}

/// Let the user point at obs64.exe with the standard Open dialog (blocks this request until closed).
/// Ok(None) = cancelled.
pub fn browse_obs(s: &Shared) -> Result<Option<PathBuf>, String> {
    let start = std::env::var("ProgramFiles").ok().map(PathBuf::from);
    match win::pick_file("Where is OBS Studio? Pick obs64.exe", ("OBS Studio", "obs64.exe"), start.as_deref()) {
        Some(f) => set_obs_path(s, Some(&f)),
        None => Ok(None),
    }
}

fn edge_path() -> Option<PathBuf> {
    for root in [win::HKCU, win::HKLM] {
        if let Some(p) = win::reg_string(root, r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe", "") {
            if Path::new(&p).exists() {
                return Some(p.into());
            }
        }
    }
    ["ProgramFiles(x86)", "ProgramFiles"].iter().filter_map(|v| std::env::var(v).ok())
        .map(|b| PathBuf::from(b).join(r"Microsoft\Edge\Application\msedge.exe")).find(|p| p.exists())
}

pub fn open_window(s: &Shared) {
    if let Some(h) = win::find_browser_window("Fragtape") {
        return win::bring_to_front(h);
    }
    let url = format!("http://127.0.0.1:{}/?t={}", s.port, s.token);
    match edge_path() {
        Some(edge) => {
            let _ = win::command(edge).arg(format!("--app={url}")).arg(format!("--user-data-dir={}", local_dir().join("window").display()))
                .args(["--window-size=1280,860", "--no-first-run", "--no-default-browser-check", "--disable-features=Translate"]).spawn();
        }
        None => {
            let _ = std::process::Command::new("explorer").arg(url).spawn();
        }
    }
}

/// The first-time question: an in-game card (see toast.rs). If it times out, the
/// question stays open in the window and the tray menu.
fn ask_card(game: &str, pids: &[u32], answers: crate::controller::Answers) -> Option<Box<dyn FnOnce() + Send>> {
    let icon = pids.first().and_then(|p| win::image_path(*p)).and_then(|p| crate::icons::exe_icon(Path::new(&p)));
    let name = game.to_string();
    Some(crate::toast::ask(game, icon, pids.to_vec(), Box::new(move |choice| {
        answers.lock().unwrap().push_back((name, Some(choice.to_string())));
    })))
}

fn game_icon(pids: &[u32]) -> Option<Vec<u8>> {
    pids.first().and_then(|p| win::image_path(*p)).and_then(|p| crate::icons::exe_icon(Path::new(&p)))
}

// ---- first run --------------------------------------------------------------------------------------
/// Take over the games, settings and clips of the OBSPlayed versions.
fn migrate() {
    let roaming = PathBuf::from(std::env::var("APPDATA").unwrap_or_default());
    let local = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default());
    let data = data_dir();
    for name in ["library.json", "gamedb.json"] {
        if !data.join(name).exists() {
            for old in [roaming.join("OBSPlayed"), roaming.join(r"obs-studio\plugin_config\obsplayed")] {
                if old.join(name).exists() && std::fs::copy(old.join(name), data.join(name)).is_ok() {
                    logf!("imported {name} from {}", old.display());
                    break;
                }
            }
        }
    }
    let (old_local, new_local) = (local.join("OBSPlayed"), local_dir());
    if !new_local.join("clips.json").exists() && old_local.join("clips.json").exists() {
        let _ = std::fs::copy(old_local.join("clips.json"), new_local.join("clips.json"));
        let _ = std::fs::create_dir_all(new_local.join("thumbs"));
        for e in std::fs::read_dir(old_local.join("thumbs")).into_iter().flatten().flatten() {
            let _ = std::fs::copy(e.path(), new_local.join("thumbs").join(e.file_name()));
        }
    }
    let old_videos = util::home().join("Videos").join("OBSPlayed");
    if old_videos.is_dir() && !videos_root().exists() && std::fs::rename(&old_videos, videos_root()).is_ok() {
        logf!("moved {} to {}", old_videos.display(), videos_root().display());
    }
    let _ = win::set_run_at_login("OBSPlayed", None); // the Python version's entry
}

fn ensure_shortcut(exe: &Path) {
    let Ok(appdata) = std::env::var("APPDATA") else { return };
    let lnk = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Fragtape.lnk");
    let ico = local_dir().join("Fragtape.ico");
    let _ = std::fs::write(&ico, tray::ico_file());
    if lnk.exists() {
        return;
    }
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.IconLocation='{},0';$s.Description='Fragtape - automatic game recording and highlights';$s.Save()",
        lnk.display(), exe.display(), exe.parent().unwrap().display(), ico.display());
    let _ = win::command("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &ps]).status();
}

// ---- main ------------------------------------------------------------------------------------------
pub fn run(show_window: bool) {
    let (data, local) = (data_dir(), local_dir());
    let _ = std::fs::create_dir_all(&data);
    let _ = std::fs::create_dir_all(&local);
    util::init_log(&data.join("app.log"));
    migrate();
    let exe = std::env::current_exe().unwrap_or_default();
    ensure_shortcut(&exe);

    let library = Arc::new(Library::open(data.join("library.json")));
    let gamedb = GameDb::new(data.clone());
    let clips = Arc::new(ClipStore::new(videos_root(), &local));
    let engine = Engine::new(local.join("engine"));
    let vendor = quality::vendor_of(&win::gpu_names());
    let (tx, rx) = channel::<Cmd>();
    let shared = Arc::new(Shared {
        token: library.token(), library: library.clone(), gamedb: gamedb.clone(), clips: clips.clone(), engine: engine.clone(),
        status: Mutex::new(json!({})), active_path: Mutex::new(None), scanning: Arc::default(),
        installing_ffmpeg: Arc::default(), installing_obs: Arc::default(), obs_note: Arc::default(), tx: Mutex::new(tx), port: PORT, vendor,
        icons: crate::icons::Icons::new(local.join("icons")),
    });
    if let Some(p) = library.get_raw("obs_path").as_str() {
        *engine.custom_obs.lock().unwrap() = Some(PathBuf::from(p));
    }
    let obs_at_start = engine.obs_dir();
    logf!("Fragtape {} starting (OBS: {}, GPU: {vendor:?})", env!("CARGO_PKG_VERSION"),
          obs_at_start.as_ref().map(|p| p.display().to_string()).unwrap_or("not found".into()));
    if library.prefs()["autostart"] == true {
        let _ = win::set_run_at_login("Fragtape", Some(&format!("\"{}\" --tray", exe.display())));
    }

    crate::toast::set_look(&library.prefs());
    server::start(shared.clone());
    let gsi = Arc::new(providers::Gsi::default());
    let s2 = shared.clone();
    gamedb.start(move || scan_async(&s2, false));

    // ---- the main loop (owns the controller) ----
    let s = shared.clone();
    let loop_thread = std::thread::Builder::new().name("main-loop".into()).spawn(move || {
        let s3 = s.clone();
        let obs = EngineObs::new(s.engine.clone(), Box::new(move |t: &Target| build_options(&s3, t)));
        let asker: Asker = Box::new(ask_card);
        let mut c = Controller::new(obs, s.library.clone(), s.gamedb.clone(), Box::new(win::running_processes), asker);
        gsi.clone().serve(c.bus.clone());
        let stopping_engine = Arc::new(AtomicBool::new(false));
        let mut quit = false;
        let mut warned_no_obs: Option<String> = None;
        while !quit {
            // commands from the window / tray
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::Answer(g, m) => c.answer(&g, m),
                    Cmd::Mark => {
                        c.mark_manual();
                        if let Some(g) = c.current.clone().filter(|_| s.library.prefs()["notify_highlight"] == true) {
                            let text = if c.obs.recording() { "Marked in the recording" } else if c.obs.replay() { "Saving the clip…" } else { "Not recording this game" };
                            let pids = c.current_pids.clone();
                            std::thread::spawn(move || crate::toast::info("HIGHLIGHT", &g.name, text, &g.name, game_icon(&pids), pids));
                        }
                    }
                    Cmd::PrefChanged(k) => {
                        let p = s.library.prefs();
                        match k.as_str() {
                            "hotkey" => tray::set_hotkey(p["hotkey"].as_str().unwrap_or("off")),
                            "theme" | "accent" | "card_corner" | "card_time" | "animations" => crate::toast::set_look(&p),
                            "text_size" | "thumb_size" => {}
                            "autostart" => {
                                let cmdline = format!("\"{}\" --tray", std::env::current_exe().unwrap_or_default().display());
                                if let Err(e) = win::set_run_at_login("Fragtape", (p["autostart"] == true).then_some(cmdline.as_str())) {
                                    logf!("could not change autostart: {e}");
                                }
                            }
                            _ if s.engine.running() => logf!("{k}: applies from the next game"),
                            _ => {}
                        }
                    }
                    Cmd::Stop => {
                        if c.obs.recording() { c.obs.stop_recording() } else if c.obs.replay() { c.obs.stop_replay() }
                    }
                    Cmd::Show => open_window(&s),
                    Cmd::Quit => quit = true,
                }
            }
            // events from the hidden OBS
            let events: Vec<(String, Value)> = s.engine.events.lock().unwrap().try_iter().collect();
            for (kind, data) in events {
                match c.obs.handle_event(&kind, &data) {
                    Some(EngineEvent::RecordingStarted) => c.on_recording_started(),
                    Some(EngineEvent::RecordingStopped(p)) => c.on_recording_stopped(p),
                    Some(EngineEvent::ReplaySaved(p)) => c.on_replay_saved(p),
                    _ => {}
                }
            }
            c.tick();
            let notify_clip = s.library.prefs()["notify_clip"] == true;
            for media in c.outbox.drain(..) {
                let store = s.clips.clone();
                let pids = c.current_pids.clone();
                std::thread::spawn(move || match media {
                    Media::Session { path, game, markers, highlights } =>
                        clips::finish_session(&store, Path::new(&path), game, &markers, &highlights),
                    Media::Clip { path, game, title } => {
                        clips::finish_clip(&store, Path::new(&path), game.clone(), &title);
                        let g = game.unwrap_or_default();
                        if notify_clip {
                            crate::toast::info("CLIP SAVED", &title, &g, &g, game_icon(&pids), pids);
                        }
                    }
                });
            }
            // a game that should be recorded, but OBS is missing: say so once per game
            let wants = c.current.is_some() && matches!(c.mode.as_deref(), Some(FULL) | Some(HIGHLIGHTS));
            let name = c.current.as_ref().map(|g| g.name.clone());
            if wants && warned_no_obs != name && !s.engine.running() && s.engine.obs_dir().is_none() {
                warned_no_obs = name.clone();
                let (g, pids) = (name.unwrap_or_default(), c.current_pids.clone());
                logf!("can't record {g}: OBS Studio isn't installed");
                std::thread::spawn(move || crate::toast::info("CAN'T RECORD", "OBS Studio is missing",
                    "Fragtape needs it to record. Open Fragtape to install it.", &g, game_icon(&pids), pids));
            }
            // close the hidden OBS when nothing needs it
            let busy = c.obs.recording() || c.obs.replay() || (c.current.is_some() && matches!(c.mode.as_deref(), Some(FULL) | Some(HIGHLIGHTS)));
            if !busy && s.engine.running() && mono() - c.obs.idle_since > IDLE_STOP_AFTER && !stopping_engine.swap(true, Ordering::Relaxed) {
                let (e, flag) = (s.engine.clone(), stopping_engine.clone());
                std::thread::spawn(move || { e.stop(false); flag.store(false, Ordering::Relaxed); });
            }
            // status for the window and the tray
            let st = c.status();
            *s.active_path.lock().unwrap() = c.obs.active_path.clone();
            let game = st["game"].as_str().unwrap_or("").to_string();
            if c.obs.recording() {
                tray::set_state("rec", &format!("Fragtape: recording {game}"));
            } else if c.obs.replay() {
                tray::set_state("clip", &format!("Fragtape: saving highlights of {game}"));
            } else if !game.is_empty() {
                tray::set_state("watching", &format!("Fragtape: {game} (not recording)"));
            } else {
                tray::set_state("watching", "Fragtape: watching for games");
            }
            *s.status.lock().unwrap() = st;
            std::thread::sleep(Duration::from_millis(500));
        }
        // shutting down: finish the recording properly first
        if c.obs.recording() {
            logf!("quitting: stopping the recording first");
            c.obs.stop_recording();
            let deadline = mono() + 15.0;
            while c.obs.recording() && mono() < deadline {
                let ev: Vec<(String, Value)> = s.engine.events.lock().unwrap().try_iter().collect();
                for (k, d) in ev {
                    if let Some(EngineEvent::RecordingStopped(p)) = c.obs.handle_event(&k, &d) {
                        c.on_recording_stopped(p);
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            for m in c.outbox.drain(..) {
                if let Media::Session { path, game, markers, highlights } = m {
                    s.clips.add(Path::new(&path), json!({"kind": "session", "game": game, "markers": markers, "highlights": highlights}));
                }
            }
        }
        if c.obs.replay() {
            c.obs.stop_replay();
        }
        s.engine.stop(false);
        tray::quit();
    }).expect("main loop");

    if show_window || obs_at_start.is_none() {
        let s = shared.clone();
        std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(300)); open_window(&s); });
    }
    let s = shared.clone();
    let menu: tray::MenuFn = Box::new(move || {
        let st = s.status.lock().unwrap().clone();
        let hotkey = s.library.prefs()["hotkey"].as_str().unwrap_or("off").to_string();
        let mut items = vec![];
        for g in st["asking"].as_array().into_iter().flatten().filter_map(|g| g.as_str()) {
            items.push((format!("New game: {g}"), None));
            for (label, mode) in [("Record whole session", FULL), ("Only highlights", HIGHLIGHTS), ("Nothing", "off")] {
                items.push((format!("    {label}"), Some(Cmd::Answer(g.to_string(), Some(mode.to_string())))));
            }
            items.push(("-".into(), None));
        }
        if let Some(g) = st["game"].as_str() {
            let what = if st["recording"] == true { "Recording" } else if st["replay"] == true { "Highlights on" } else { "Playing" };
            items.push((format!("{what}: {g}"), None));
            items.push((format!("Save highlight ({hotkey})"), Some(Cmd::Mark)));
        }
        if st["recording"] == true {
            items.push(("Stop recording".into(), Some(Cmd::Stop)));
        }
        items
    });
    let s = shared.clone();
    let hotkey = library.prefs()["hotkey"].as_str().unwrap_or("off").to_string();
    tray::run(Box::new(move |c| s.send(c)), menu, &hotkey);
    // tray closed: make sure the loop finishes the recording and closes OBS
    shared.send(Cmd::Quit);
    let _ = loop_thread.join();
    logf!("bye");
}
