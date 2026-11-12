//! The recording engine: a private, hidden copy of the installed OBS Studio.
//!
//! Runs in portable mode from %LOCALAPPDATA%\Fragtape\engine (directory junctions
//! to the OBS install, nothing copied) with its own settings, no preview, minimized
//! to the tray, and only while a game is recorded. Controlled over obs-websocket
//! with a random password that changes every start.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::obsws::ObsWs;
use crate::util::{mono, token};
use crate::{logf, quality, win};

pub const SCENE: &str = "Fragtape";
pub const GAME: &str = "Game";
pub const WINDOW: &str = "Game Window";
pub const SCREEN: &str = "Screen";
pub const GAME_AUDIO: &str = "Game Audio";
pub const DESKTOP: &str = "Desktop Audio";
pub const MIC: &str = "Mic/Aux";
const PROFILE: &str = "Fragtape";
const FORCE_MONO: i64 = 1 << 1; // OBS_SOURCE_FLAG_FORCE_MONO

/// Everything the hidden OBS is configured with.
#[derive(Clone, Debug)]
pub struct Options {
    pub rec_dir: PathBuf,
    pub encoder: String,
    pub encoder_settings: Value,
    pub fps: i64,
    pub base: (u32, u32),
    pub out: (u32, u32),
    pub scale_filter: String,
    pub color: String, // "NV12" / "P010"
    pub capture: String,
    /// The monitor for the whole-screen capture (OBS's `monitor_id`).
    pub monitor_id: String,
    pub audio: String,
    pub mic_mono: bool,
    /// Extra microphone gain in dB (a limiter after it stops clipping).
    pub mic_boost: i64,
    pub replay_seconds: i64,
    pub replay_mb: i64,
    pub window_spec: String,
    pub audio_bitrate: i64,
    pub sample_rate: i64,
    pub separate_tracks: bool,
    pub format: String,
    pub priority: String,
}

/// Turn whatever the user picked (the OBS folder, its bin folder or obs64.exe) into the OBS root.
pub fn obs_root(picked: &Path) -> Result<PathBuf, String> {
    let root = picked.ancestors().take(4)
        .find(|p| p.join("bin").join("64bit").join("obs64.exe").exists())
        .ok_or(r"that isn't an OBS Studio folder (no bin\64bit\obs64.exe)")?;
    let ws = [root.join(r"obs-plugins\64bit\obs-websocket.dll"), root.join(r"core\obs-websocket\obs-websocket.dll")];
    if !ws.iter().any(|p| p.exists()) {
        return Err("this OBS is too old: Fragtape needs OBS Studio 28 or newer".into());
    }
    Ok(root.to_path_buf())
}

pub fn find_obs() -> Option<PathBuf> {
    if std::env::var_os("FRAGTAPE_PRETEND_NO_OBS").is_some() {
        return None; // for testing the setup screens on a PC that has OBS
    }
    let mut c = vec![];
    for root in [win::HKLM, win::HKCU] {
        if let Some(p) = win::reg_string(root, r"SOFTWARE\OBS Studio", "") {
            c.push(PathBuf::from(p));
        }
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(p) = std::env::var(var) {
            c.push(PathBuf::from(p).join("obs-studio"));
        }
    }
    c.into_iter().find(|p| obs_root(p).is_ok())
}

/// Which sources are muted for each audio choice. "screen" / "screen_mic" are for screen
/// recordings (no game window to take the sound from: the whole PC sound instead).
pub fn audio_mutes(audio: &str) -> [(&'static str, bool); 3] {
    let screen = audio.starts_with("screen");
    [(DESKTOP, audio != "all" && !screen), (MIC, audio == "game" || audio == "screen"), (GAME_AUDIO, audio == "all" || screen)]
}

/// Scene items: 1 = window capture, 2 = game capture, 3 = the whole screen.
pub fn capture_items(capture: &str) -> [(i64, bool); 3] {
    let screen = capture == "screen";
    [(1, !screen), (2, !screen), (3, screen)]
}

fn ini(sections: &[(&str, Vec<(&str, String)>)]) -> String {
    let mut s = String::new();
    for (name, kv) in sections {
        s += &format!("[{name}]\n");
        for (k, v) in kv {
            s += &format!("{k}={v}\n");
        }
        s += "\n";
    }
    s
}

fn source(name: &str, kind: &str, settings: Value, muted: bool, flags: i64, mixers: i64) -> Value {
    let uuid = {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(format!("fragtape/{name}"));
        let x: String = h.iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
    };
    json!({"name": name, "uuid": uuid, "id": kind, "versioned_id": kind, "settings": settings, "mixers": mixers,
           "sync": 0, "flags": flags, "volume": 1.0, "balance": 0.5, "enabled": true, "muted": muted,
           "monitoring_type": 0, "private_settings": {}})
}

fn item(src: &Value, id: i64, (w, h): (u32, u32), visible: bool) -> Value {
    json!({"name": src["name"], "source_uuid": src["uuid"], "visible": visible, "locked": true, "rot": 0.0,
           "pos": {"x": 0.0, "y": 0.0}, "scale": {"x": 1.0, "y": 1.0}, "align": 5,
           "bounds_type": 2, "bounds_align": 0, "bounds": {"x": w as f64, "y": h as f64}, // 2 = scale to fit inside
           "crop_left": 0, "crop_top": 0, "crop_right": 0, "crop_bottom": 0, "id": id, "group_item_backup": false,
           "scale_filter": "disable", "blend_method": "default", "blend_type": "normal", "private_settings": {}})
}

pub fn scene_collection(o: &Options) -> Value {
    let mutes = audio_mutes(&o.audio);
    let muted = |n: &str| mutes.iter().find(|(k, _)| *k == n).map(|(_, m)| *m).unwrap_or(false);
    // Separate tracks: 1 = everything, 2 = game, 3 = mic, 4 = desktop.
    let (game_mix, mic_mix, desk_mix) = if o.separate_tracks { (1 | 2, 1 | 4, 1 | 8) } else { (255, 255, 255) };
    let spec = &o.window_spec;
    let window = source(WINDOW, "window_capture", json!({"window": spec, "priority": 2, "method": 2, "cursor": false,
                                                         "client_area": true, "compatibility": false}), false, 0, 0);
    let game = source(GAME, "game_capture", json!({"capture_mode": "window", "window": spec, "priority": 2,
                                                   "capture_cursor": false, "allow_transparency": false,
                                                   "anti_cheat_hook": true, "capture_audio": false}), false, 0, 0);
    let screen = source(SCREEN, "monitor_capture", json!({"monitor_id": o.monitor_id, "capture_cursor": true}), false, 0, 0);
    let audio = source(GAME_AUDIO, "wasapi_process_output_capture", json!({"window": spec, "priority": 2}),
                       muted(GAME_AUDIO), 0, game_mix);
    let screen_mode = o.capture == "screen";
    let mut mic = source(MIC, "wasapi_input_capture", json!({"device_id": "default"}), muted(MIC),
                         if o.mic_mono { FORCE_MONO } else { 0 }, mic_mix);
    if o.mic_boost > 0 {
        mic["filters"] = json!([
            source("Fragtape mic boost", "gain_filter", json!({"db": o.mic_boost as f64}), false, 0, 0),
            source("Fragtape mic limiter", "limiter_filter", json!({"threshold": -1.0, "release_time": 60}), false, 0, 0),
        ]);
    }
    // First item = bottom layer: Game Capture covers the window capture once it hooks the game.
    let scene = source(SCENE, "scene", json!({"id_counter": 4, "custom_size": false, "items": [
        item(&window, 1, o.base, !screen_mode), item(&game, 2, o.base, !screen_mode),
        item(&screen, 3, o.base, screen_mode), item(&audio, 4, o.base, true)]}), false, 0, 0);
    json!({
        "name": PROFILE,
        "DesktopAudioDevice1": source(DESKTOP, "wasapi_output_capture", json!({"device_id": "default"}), muted(DESKTOP), 0, desk_mix),
        "AuxAudioDevice1": mic,
        "sources": [scene, window, game, screen, audio],
        "groups": [], "scene_order": [{"name": SCENE}], "current_scene": SCENE, "current_program_scene": SCENE,
        "current_transition": "Cut", "transition_duration": 0, "transitions": [], "quick_transitions": [],
        "saved_projectors": [], "preview_locked": true, "scaling_enabled": false, "modules": {"scripts-tool": []}, "version": 2,
    })
}

pub fn write_config(cfg: &Path, o: &Options, ws_port: u16, ws_password: &str) -> std::io::Result<()> {
    let root = cfg.join("obs-studio");
    let prof = root.join("basic").join("profiles").join(PROFILE);
    for d in [prof.clone(), root.join("basic").join("scenes"), root.join("plugin_config").join("obs-websocket")] {
        fs::create_dir_all(d)?;
    }
    let f = |s: &str| s.to_string();
    let priority = match o.priority.as_str() { "high" => "High", "above_normal" => "AboveNormal", _ => "Normal" };
    fs::write(root.join("user.ini"), ini(&[
        ("General", vec![("FirstRun", f("true")), ("ConfirmOnExit", f("false")), ("Pre19Defaults", f("false")),
                         ("Pre21Defaults", f("false")), ("Pre23Defaults", f("false")), ("Pre24.1Defaults", f("false")),
                         ("EnableAutoUpdates", f("false")), ("WarnBeforeStoppingRecord", f("false")),
                         ("ProcessPriority", f(priority))]),
        // Never set SysTrayMinimizeToTray: it makes OBS ignore WM_CLOSE.
        ("BasicWindow", vec![("PreviewEnabled", f("false")), ("SysTrayEnabled", f("true")), ("SysTrayWhenStarted", f("true")),
                             ("ShowTransitions", f("false"))]),
        ("Basic", vec![("Profile", f(PROFILE)), ("ProfileDir", f(PROFILE)), ("SceneCollection", f(PROFILE)),
                       ("SceneCollectionFile", format!("{PROFILE}.json")), ("ConfigOnNewProfile", f("false"))]),
    ]))?;
    if !root.join("global.ini").exists() {
        fs::write(root.join("global.ini"), "")?;
    }
    let rec_dir = o.rec_dir.to_string_lossy().replace('\\', "\\\\");
    let tracks = if o.separate_tracks { 1 | 2 | 4 | 8 } else { 1 };
    let ab = o.audio_bitrate.to_string();
    fs::write(prof.join("basic.ini"), ini(&[
        ("General", vec![("Name", f(PROFILE))]),
        ("Output", vec![("Mode", f("Advanced")), ("FilenameFormatting", f("%CCYY-%MM-%DD %hh-%mm-%ss"))]),
        ("SimpleOutput", vec![("RecRBPrefix", f("Clip")), ("FilePath", rec_dir.clone()), ("RecFormat2", o.format.clone())]),
        ("AdvOut", vec![
            ("RecType", f("Standard")), ("RecFilePath", rec_dir), ("RecFormat2", o.format.clone()),
            ("RecEncoder", o.encoder.clone()), ("RecAudioEncoder", f("ffmpeg_aac")), ("RecTracks", tracks.to_string()),
            ("TrackIndex", f("1")), ("Encoder", f("obs_x264")), ("RecUseRescale", f("false")),
            ("Track1Bitrate", ab.clone()), ("Track2Bitrate", ab.clone()), ("Track3Bitrate", ab.clone()), ("Track4Bitrate", ab),
            ("Track1Name", f("Everything")), ("Track2Name", f("Game")), ("Track3Name", f("Microphone")), ("Track4Name", f("Desktop")),
            ("RecRB", f("true")), ("RecRBTime", o.replay_seconds.to_string()), ("RecRBSize", o.replay_mb.to_string()),
            ("RecRBPrefix", f("Clip")),
        ]),
        ("Video", vec![
            ("BaseCX", o.base.0.to_string()), ("BaseCY", o.base.1.to_string()),
            ("OutputCX", o.out.0.to_string()), ("OutputCY", o.out.1.to_string()),
            ("FPSType", f("1")), ("FPSInt", o.fps.to_string()), // integer fps: any value (144, 165, ...)
            ("ScaleType", o.scale_filter.clone()), ("ColorFormat", o.color.clone()), ("ColorSpace", f("709")),
            ("ColorRange", f("Partial")),
        ]),
        ("Audio", vec![("SampleRate", o.sample_rate.to_string()), ("ChannelSetup", f("Stereo"))]),
    ]))?;
    fs::write(prof.join("recordEncoder.json"), o.encoder_settings.to_string())?;
    fs::write(root.join("basic").join("scenes").join(format!("{PROFILE}.json")), serde_json::to_vec_pretty(&scene_collection(o))?)?;
    fs::write(root.join("plugin_config").join("obs-websocket").join("config.json"), serde_json::to_vec_pretty(&json!({
        "alerts_enabled": false, "auth_required": true, "first_load": false, "server_enabled": true,
        "server_password": ws_password, "server_port": ws_port,
    }))?)?;
    Ok(())
}

fn junction(link: &Path, target: &Path) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(link) {
        if fs::canonicalize(link).ok() == fs::canonicalize(target).ok() {
            return Ok(());
        }
        if meta.is_dir() && !meta.file_type().is_symlink() && fs::read_link(link).is_err() {
            // A real folder from an older layout: it only ever holds junctions.
            for e in fs::read_dir(link).into_iter().flatten().flatten() {
                let _ = fs::remove_dir(e.path());
            }
        }
        fs::remove_dir(link).map_err(|e| format!("{}: {e}", link.display()))?; // removes the link, not the target
    }
    let ok = win::command("cmd").args(["/c", "mklink", "/J"]).arg(link).arg(target).output()
        .map_err(|e| e.to_string())?.status.success();
    if ok { Ok(()) } else { Err(format!("could not link {}", link.display())) }
}

/// Build the portable engine folder; returns obs64.exe inside it.
pub fn ensure_layout(obs: &Path, dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for d in ["bin", "data", "core"] {
        if obs.join(d).exists() {
            junction(&dir.join(d), &obs.join(d))?;
        }
    }
    if obs.join("core").exists() {
        let _ = fs::create_dir_all(dir.join("obs-plugins")); // OBS 33+: leave out third-party plugins
    } else {
        junction(&dir.join("obs-plugins"), &obs.join("obs-plugins"))?; // OBS before 33
    }
    let _ = fs::create_dir_all(dir.join("config"));
    Ok(dir.join("bin").join("64bit").join("obs64.exe"))
}

fn free_port(preferred: u16) -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", preferred)).or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))
        .and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(preferred)
}

/// Video encoders listed in the hidden OBS's newest log ("\t- obs_nvenc_h264_tex (NVIDIA NVENC H.264)").
pub fn encoders_from_log(log: &str) -> Vec<String> {
    let mut out = vec![];
    let mut in_video = false;
    for line in log.lines() {
        let body = line.split_once(": ").map(|(_, b)| b).unwrap_or(line);
        if body.trim() == "Video Encoders:" {
            in_video = true;
        } else if in_video {
            match body.trim().strip_prefix("- ") {
                Some(rest) => out.push(rest.split_whitespace().next().unwrap_or("").to_string()),
                None => break,
            }
        }
    }
    out
}

pub struct Engine {
    /// A folder the user chose in Settings (None = find the installed OBS).
    pub custom_obs: Mutex<Option<PathBuf>>,
    pub dir: PathBuf,
    password: String,
    proc: Mutex<Option<Child>>,
    ws: Mutex<Option<Arc<ObsWs>>>,
    tx: Sender<(String, Value)>,
    pub events: Mutex<Receiver<(String, Value)>>,
    pub encoders: Mutex<Option<Vec<String>>>,
}

impl Engine {
    pub fn new(dir: PathBuf) -> Arc<Self> {
        let (tx, rx) = channel();
        let encoders = crate::util::read_json(&dir.join("encoders.json"))
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok());
        Arc::new(Engine { custom_obs: Mutex::new(None), dir, password: token(18), proc: Mutex::new(None), ws: Mutex::new(None),
                          tx, events: Mutex::new(rx), encoders: Mutex::new(encoders) })
    }

    /// The OBS to use: the chosen folder if it is still valid, else the installed one.
    /// Checked on every call, so installing OBS takes effect without restarting.
    pub fn obs_dir(&self) -> Option<PathBuf> {
        let custom = self.custom_obs.lock().unwrap().clone();
        custom.and_then(|p| obs_root(&p).ok()).or_else(find_obs)
    }

    pub fn running(&self) -> bool {
        let alive = self.proc.lock().unwrap().as_mut().is_some_and(|p| p.try_wait().ok().flatten().is_none());
        alive && self.ws.lock().unwrap().as_ref().is_some_and(|w| !w.is_closed())
    }

    pub fn start(&self, o: &Options) -> Result<(), String> {
        if self.running() {
            return Ok(());
        }
        let obs = self.obs_dir().ok_or("OBS Studio is not installed (obsproject.com)")?;
        self.stop(true);
        let exe = ensure_layout(&obs, &self.dir)?;
        let port = free_port(4466);
        write_config(&self.dir.join("config"), o, port, &self.password).map_err(|e| e.to_string())?;
        let t0 = Instant::now();
        let child = win::command(&exe)
            .args(["--portable", "--multi", "--minimize-to-tray", "--disable-updater", "--disable-shutdown-check",
                   "--disable-missing-files-check"])
            .current_dir(exe.parent().unwrap()).spawn().map_err(|e| format!("could not start OBS: {e}"))?;
        let pid = child.id();
        *self.proc.lock().unwrap() = Some(child);
        let deadline = t0 + Duration::from_secs(40);
        let mut last = String::new();
        let ws = loop {
            if Instant::now() > deadline {
                self.stop(true);
                return Err(format!("could not connect to the recording engine: {last}"));
            }
            if self.proc.lock().unwrap().as_mut().is_some_and(|p| p.try_wait().ok().flatten().is_some()) {
                return Err("OBS exited during startup".into());
            }
            let tx = self.tx.clone();
            let tx2 = self.tx.clone();
            match ObsWs::connect(port, &self.password, move |t, d| { let _ = tx.send((t.to_string(), d.clone())); },
                                 move || { let _ = tx2.send(("EngineClosed".into(), Value::Null)); }) {
                Ok(ws) => break Arc::new(ws),
                Err(e) => { last = e; std::thread::sleep(Duration::from_millis(400)); }
            }
        };
        // obs-websocket answers "not ready" until OBS has loaded its scenes.
        loop {
            match ws.request("GetRecordStatus", None, Duration::from_secs(3)) {
                Ok(_) => break,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(300)),
                Err(e) => { self.stop(true); return Err(e); }
            }
        }
        *self.ws.lock().unwrap() = Some(ws);
        logf!("[engine] OBS started in {:.1}s (pid {pid}, {} {}x{} {} fps)", t0.elapsed().as_secs_f64(),
              quality::encoder_name(&o.encoder), o.out.0, o.out.1, o.fps);
        self.read_encoders();
        Ok(())
    }

    fn read_encoders(&self) {
        let logs = self.dir.join("config").join("obs-studio").join("logs");
        let newest = fs::read_dir(&logs).into_iter().flatten().flatten()
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        if let Some(e) = newest {
            let list = encoders_from_log(&fs::read_to_string(e.path()).unwrap_or_default());
            if !list.is_empty() {
                let _ = fs::write(self.dir.join("encoders.json"), serde_json::to_vec(&list).unwrap());
                *self.encoders.lock().unwrap() = Some(list);
            }
        }
    }

    pub fn stop(&self, force: bool) {
        if let Some(ws) = self.ws.lock().unwrap().take() {
            ws.close();
        }
        let Some(mut child) = self.proc.lock().unwrap().take() else { return };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(if force { 0 } else { 15 });
        while Instant::now() < deadline {
            // WM_CLOSE to OBS's main window, like clicking Exit (taskkill doesn't work: it hits Qt helper windows).
            win::close_windows(child.id(), "OBS ");
            for _ in 0..25 {
                if child.try_wait().ok().flatten().is_some() {
                    return logf!("[engine] OBS closed");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let _ = child.kill(); // --disable-shutdown-check: no "crash detected" prompt next time
        let _ = child.wait();
        if !force {
            logf!("[engine] OBS did not close in time; stopped it");
        }
    }

    pub fn request(&self, kind: &str, data: Option<Value>, timeout_s: u64) -> Result<Value, String> {
        let ws = self.ws.lock().unwrap().clone().ok_or("engine not running")?;
        ws.request(kind, data, Duration::from_secs(timeout_s))
    }
}

/// What to record: the game's window (OBS window string), name and processes.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub spec: Option<String>,
    pub game: String,
    pub pids: Vec<u32>,
    /// A screen recording started by the user (the whole monitor, not a game).
    pub screen: bool,
}

/// What the controller needs from OBS (implemented by the engine, faked in tests).
pub trait Obs {
    fn recording(&self) -> bool;
    fn replay(&self) -> bool;
    fn start_recording(&mut self, target: Target);
    fn stop_recording(&mut self);
    fn add_chapter(&mut self, name: &str) -> bool;
    fn start_replay(&mut self, target: Target);
    fn stop_replay(&mut self);
    fn save_replay(&mut self);
    fn update_window(&mut self, spec: &str);
}

/// The controller's view of the hidden OBS.
pub struct EngineObs {
    pub engine: Arc<Engine>,
    pub options: Box<dyn Fn(&Target) -> Options + Send>,
    pub recording: bool,
    pub replay: bool,
    pub active_path: Option<String>,
    pub last_recording: Option<String>,
    pub last_replay: Option<String>,
    pub idle_since: f64,
    target: Target,
}

pub enum EngineEvent {
    RecordingStarted,
    RecordingStopped(Option<String>),
    ReplaySaved(Option<String>),
    ReplayStopped,
}

impl EngineObs {
    pub fn new(engine: Arc<Engine>, options: Box<dyn Fn(&Target) -> Options + Send>) -> Self {
        EngineObs { engine, options, recording: false, replay: false, active_path: None, last_recording: None,
                    last_replay: None, idle_since: mono(), target: Target::default() }
    }

    pub fn handle_event(&mut self, kind: &str, d: &Value) -> Option<EngineEvent> {
        let state = d["outputState"].as_str().unwrap_or("");
        let path = d["outputPath"].as_str().map(String::from);
        match (kind, state) {
            ("RecordStateChanged", "OBS_WEBSOCKET_OUTPUT_STARTED") => {
                self.recording = true;
                self.active_path = path;
                Some(EngineEvent::RecordingStarted)
            }
            ("RecordStateChanged", "OBS_WEBSOCKET_OUTPUT_STOPPED") => {
                self.recording = false;
                if std::mem::take(&mut self.target.screen) && self.engine.running() {
                    let _ = self.ensure(); // back to the game's capture (the replay buffer may still run)
                }
                self.active_path = None;
                self.idle_since = mono();
                if path.is_some() {
                    self.last_recording = path;
                }
                Some(EngineEvent::RecordingStopped(self.last_recording.clone()))
            }
            ("ReplayBufferStateChanged", "OBS_WEBSOCKET_OUTPUT_STARTED") => { self.replay = true; None }
            ("ReplayBufferStateChanged", "OBS_WEBSOCKET_OUTPUT_STOPPED") => {
                self.replay = false;
                self.idle_since = mono();
                Some(EngineEvent::ReplayStopped)
            }
            ("ReplayBufferSaved", _) => {
                self.last_replay = d["savedReplayPath"].as_str().map(String::from);
                Some(EngineEvent::ReplaySaved(self.last_replay.clone()))
            }
            ("EngineClosed", _) => {
                let was = self.recording;
                (self.recording, self.replay, self.active_path) = (false, false, None);
                self.target.screen = false;
                self.idle_since = mono();
                if was {
                    logf!("[engine] OBS closed while recording");
                    return Some(EngineEvent::RecordingStopped(self.last_recording.clone()));
                }
                None
            }
            _ => None,
        }
    }

    fn ensure(&mut self) -> Result<(), String> {
        let o = (self.options)(&self.target);
        let _ = fs::create_dir_all(&o.rec_dir);
        if self.engine.running() {
            let dir = o.rec_dir.to_string_lossy().to_string();
            self.engine.request("SetRecordDirectory", Some(json!({"recordDirectory": dir})), 5)?;
            self.update_window(&o.window_spec.clone());
            for (name, muted) in audio_mutes(&o.audio) {
                let _ = self.engine.request("SetInputMute", Some(json!({"inputName": name, "inputMuted": muted})), 5);
            }
            let _ = self.engine.request("SetInputSettings", Some(json!({"inputName": SCREEN, "inputSettings": {"monitor_id": o.monitor_id}})), 5);
            for (id, on) in capture_items(&o.capture) {
                let _ = self.engine.request("SetSceneItemEnabled", Some(json!({"sceneName": SCENE, "sceneItemId": id, "sceneItemEnabled": on})), 5);
            }
            Ok(())
        } else {
            self.engine.start(&o)
        }
    }
}

impl Obs for EngineObs {
    fn recording(&self) -> bool { self.recording }
    fn replay(&self) -> bool { self.replay }

    fn start_recording(&mut self, target: Target) {
        if target.screen {
            self.target.screen = true; // keep the game's window for when it ends
        } else {
            self.target = target;
        }
        match self.ensure().and_then(|_| self.engine.request("StartRecord", None, 10)) {
            Ok(_) => self.recording = true,
            Err(e) => logf!("[engine] could not start recording: {e}"),
        }
    }

    fn stop_recording(&mut self) {
        match self.engine.request("StopRecord", None, 30) {
            Ok(r) => if let Some(p) = r["outputPath"].as_str() { self.last_recording = Some(p.into()) },
            Err(e) => logf!("[engine] could not stop recording: {e}"),
        }
    }

    fn add_chapter(&mut self, name: &str) -> bool {
        self.engine.request("CreateRecordChapter", Some(json!({"chapterName": name})), 5).is_ok()
    }

    fn start_replay(&mut self, target: Target) {
        let screen = self.recording && self.target.screen;
        self.target = Target { screen, ..target };
        match self.ensure().and_then(|_| self.engine.request("StartReplayBuffer", None, 10)) {
            Ok(_) => self.replay = true,
            Err(e) => logf!("[engine] could not start the replay buffer: {e}"),
        }
    }

    fn stop_replay(&mut self) {
        if let Err(e) = self.engine.request("StopReplayBuffer", None, 10) {
            logf!("[engine] could not stop the replay buffer: {e}");
        }
    }

    fn save_replay(&mut self) {
        if let Err(e) = self.engine.request("SaveReplayBuffer", None, 10) {
            logf!("[engine] could not save the clip: {e}");
        }
    }

    fn update_window(&mut self, spec: &str) {
        self.target.spec = Some(spec.to_string());
        if !self.engine.running() || self.target.screen {
            return;
        }
        for name in [GAME, WINDOW, GAME_AUDIO] {
            if let Err(e) = self.engine.request("SetInputSettings", Some(json!({"inputName": name, "inputSettings": {"window": spec}})), 5) {
                logf!("[engine] could not update {name}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn options() -> Options {
        Options {
            rec_dir: PathBuf::from(r"C:\Videos\Fragtape\Game"), encoder: "obs_nvenc_h264_tex".into(),
            encoder_settings: json!({"rate_control": "CQP"}), fps: 144, base: (2560, 1440), out: (1920, 1080),
            scale_filter: "lanczos".into(), color: "NV12".into(), capture: "game".into(), monitor_id: "DISPLAY1".into(), audio: "game_mic".into(),
            mic_mono: true, mic_boost: 12, replay_seconds: 30, replay_mb: 2048, window_spec: "T:C:game.exe".into(), audio_bitrate: 192,
            sample_rate: 48000, separate_tracks: true, format: "hybrid_mp4".into(), priority: "above_normal".into(),
        }
    }

    #[test]
    fn screen_recordings_take_the_whole_pc_sound() {
        let m = |a: &str| audio_mutes(a).map(|(_, muted)| muted); // desktop, mic, game audio
        assert_eq!(m("screen_mic"), [false, false, true]);
        assert_eq!(m("screen"), [false, true, true]);
        assert_eq!(m("game"), [true, true, false]);
        assert_eq!(capture_items("screen"), [(1, false), (2, false), (3, true)]);
    }

    #[test]
    fn writes_config() {
        let dir = std::env::temp_dir().join(format!("fragtape-engine-{}", token(6)));
        write_config(&dir, &options(), 4466, "secret").unwrap();
        let root = dir.join("obs-studio");
        let basic = fs::read_to_string(root.join("basic/profiles/Fragtape/basic.ini")).unwrap();
        for needle in ["RecEncoder=obs_nvenc_h264_tex", "BaseCX=2560", "OutputCX=1920", "FPSInt=144", "RecTracks=15",
                       "Track1Bitrate=192", "RecFormat2=hybrid_mp4", "ScaleType=lanczos", "RecRBSize=2048", "RecFilePath=C:\\\\Videos"] {
            assert!(basic.contains(needle), "{needle}");
        }
        let user = fs::read_to_string(root.join("user.ini")).unwrap();
        assert!(user.contains("PreviewEnabled=false") && user.contains("ProcessPriority=AboveNormal") && !user.contains("SysTrayMinimizeToTray"));
        let ws: Value = serde_json::from_slice(&fs::read(root.join("plugin_config/obs-websocket/config.json")).unwrap()).unwrap();
        assert_eq!((ws["server_port"].clone(), ws["server_password"].clone()), (json!(4466), json!("secret")));
        let scene: Value = serde_json::from_slice(&fs::read(root.join("basic/scenes/Fragtape.json")).unwrap()).unwrap();
        assert_eq!(scene["AuxAudioDevice1"]["flags"], FORCE_MONO);
        assert_eq!((scene["AuxAudioDevice1"]["muted"].clone(), scene["DesktopAudioDevice1"]["muted"].clone()), (json!(false), json!(true)));
        assert_eq!(scene["AuxAudioDevice1"]["mixers"], 5); // track 1 + mic track
        assert_eq!(scene["AuxAudioDevice1"]["filters"][0]["id"], "gain_filter");
        assert_eq!(scene["AuxAudioDevice1"]["filters"][0]["settings"]["db"], 12.0);
        assert_eq!(scene["AuxAudioDevice1"]["filters"][1]["id"], "limiter_filter");
        let items: Vec<&str> = scene["sources"][0]["settings"]["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(items, vec![WINDOW, GAME, SCREEN, GAME_AUDIO]);
        assert_eq!(scene["sources"][0]["settings"]["items"][0]["bounds"]["x"], 2560.0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn obs_root_accepts_folder_bin_or_exe() {
        let root = std::env::temp_dir().join(format!("fragtape-obsroot-{}", std::process::id()));
        let exe = root.join("bin").join("64bit").join("obs64.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        assert!(obs_root(&root).unwrap_err().contains("too old"));
        fs::create_dir_all(root.join("core").join("obs-websocket")).unwrap();
        fs::write(root.join("core").join("obs-websocket").join("obs-websocket.dll"), b"").unwrap();
        for picked in [root.clone(), root.join("bin"), exe.clone()] {
            assert_eq!(obs_root(&picked).unwrap(), root);
        }
        assert!(obs_root(&std::env::temp_dir()).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_encoders_from_log() {
        let log = "10:00:00.000: Available Encoders:\n10:00:00.000:   Video Encoders:\n10:00:00.000: \t- obs_nvenc_h264_tex (NVIDIA NVENC H.264)\n10:00:00.000: \t- obs_nvenc_av1_tex (NVIDIA NVENC AV1)\n10:00:00.000:   Audio Encoders:\n10:00:00.000: \t- ffmpeg_aac (FFmpeg AAC)\n";
        assert_eq!(encoders_from_log(log), vec!["obs_nvenc_h264_tex", "obs_nvenc_av1_tex"]);
    }

    #[test]
    fn engine_events() {
        let mut api = EngineObs::new(Engine::new(std::env::temp_dir()), Box::new(|_: &Target| options()));
        assert!(matches!(api.handle_event("RecordStateChanged", &json!({"outputState": "OBS_WEBSOCKET_OUTPUT_STARTED", "outputPath": "a.mp4"})), Some(EngineEvent::RecordingStarted)));
        assert_eq!(api.active_path.as_deref(), Some("a.mp4"));
        assert!(matches!(api.handle_event("RecordStateChanged", &json!({"outputState": "OBS_WEBSOCKET_OUTPUT_STOPPED", "outputPath": "a.mp4"})), Some(EngineEvent::RecordingStopped(Some(_)))));
        assert!(!api.recording && api.active_path.is_none());
        api.recording = true;
        assert!(matches!(api.handle_event("EngineClosed", &Value::Null), Some(EngineEvent::RecordingStopped(_))));
    }
}
