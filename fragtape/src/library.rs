//! The user's games (modes, play history) and settings, stored in library.json.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::util::{read_json, token, unix_now, write_atomic};

pub const ASK: &str = "ask";
pub const FULL: &str = "full";
pub const HIGHLIGHTS: &str = "highlights";
pub const OFF: &str = "off";
pub const MODES: [&str; 4] = [ASK, FULL, HIGHLIGHTS, OFF];

/// What values a setting accepts.
pub enum Spec {
    Choice(&'static [&'static str]),
    Int(&'static [i64]),
    Range(i64, i64),
    Bool,
}

/// Every setting: (key, accepted values, default).
pub fn pref_specs() -> Vec<(&'static str, Spec, Value)> {
    use Spec::*;
    vec![
        ("capture", Choice(&["game", "screen"]), json!("game")),
        ("audio", Choice(&["game", "game_mic", "all"]), json!("game")),
        ("mic_mono", Bool, json!(true)),
        ("quality", Choice(&["small", "balanced", "high", "max", "custom"]), json!("high")),
        ("fps", Int(&[30, 60]), json!(60)),
        ("clip_length", Int(&[15, 30, 60, 120]), json!(30)),
        ("hotkey", Choice(&["F8", "F9", "F10", "Ctrl+F8", "off"]), json!("F8")),
        ("autostart", Bool, json!(false)),
        ("theme", Choice(&["sharp", "win10dark", "metro", "win10light", "vista"]), json!("sharp")),
        ("accent", Choice(&["theme", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"]), json!("theme")),
        ("text_size", Choice(&["small", "normal", "large"]), json!("normal")),
        ("thumb_size", Choice(&["small", "medium", "large"]), json!("medium")),
        ("animations", Bool, json!(true)),
        ("card_corner", Choice(&["top_right", "top_left", "bottom_right", "bottom_left"]), json!("top_right")),
        ("card_time", Choice(&["short", "normal", "long"]), json!("normal")),
        ("notify_highlight", Bool, json!(true)), // "Highlight marked" card when the hotkey is pressed
        ("notify_clip", Bool, json!(true)),      // "Clip saved" card
        // ---- advanced (used when quality = "custom", the rest always) ----
        ("adv_codec", Choice(&["h264", "hevc", "av1"]), json!("h264")),
        ("adv_encoder", Choice(&["auto", "gpu", "x264"]), json!("auto")),
        ("adv_rate_control", Choice(&["cqp", "cbr", "vbr"]), json!("cqp")),
        ("adv_bitrate", Range(1000, 200_000), json!(30000)),
        ("adv_max_bitrate", Range(1000, 300_000), json!(45000)),
        ("adv_cq", Range(1, 51), json!(20)),
        ("adv_preset", Range(1, 7), json!(5)),
        ("adv_keyint", Range(1, 10), json!(2)),
        ("adv_bframes", Range(0, 4), json!(2)),
        ("adv_lookahead", Bool, json!(true)),
        ("adv_resolution", Int(&[0, 2160, 1440, 1080, 900, 720]), json!(0)),
        ("adv_scale_filter", Choice(&["bicubic", "lanczos", "bilinear", "area"]), json!("lanczos")),
        ("adv_fps", Int(&[0, 24, 30, 48, 60, 90, 120, 144, 165, 240]), json!(0)),
        ("adv_audio_bitrate", Int(&[96, 128, 160, 192, 256, 320]), json!(192)),
        ("adv_sample_rate", Int(&[44100, 48000]), json!(48000)),
        ("adv_tracks", Choice(&["single", "separate"]), json!("single")),
        ("adv_format", Choice(&["hybrid_mp4", "hybrid_mov", "mkv", "mp4"]), json!("hybrid_mp4")),
        ("adv_color", Choice(&["nv12", "p010"]), json!("nv12")),
        ("adv_replay_mb", Int(&[512, 1024, 2048, 4096, 8192]), json!(2048)),
        ("adv_priority", Choice(&["normal", "above_normal", "high"]), json!("above_normal")),
    ]
}

fn valid(spec: &Spec, v: &Value) -> bool {
    match spec {
        Spec::Choice(c) => v.as_str().is_some_and(|s| c.contains(&s)),
        Spec::Int(c) => v.as_i64().is_some_and(|n| c.contains(&n)),
        Spec::Range(lo, hi) => v.as_i64().is_some_and(|n| (*lo..=*hi).contains(&n)),
        Spec::Bool => v.is_boolean(),
    }
}

fn coerce(spec: &Spec, v: &Value) -> Value {
    match spec {
        Spec::Bool => match v {
            Value::String(s) => json!(s == "true" || s == "1"),
            Value::Number(n) => json!(n.as_i64() == Some(1)),
            other => other.clone(),
        },
        Spec::Int(_) | Spec::Range(..) => match v {
            Value::String(s) => s.trim().parse::<i64>().map(|n| json!(n)).unwrap_or(Value::Null),
            Value::Number(n) => n.as_f64().map(|f| json!(f.round() as i64)).unwrap_or(Value::Null),
            other => other.clone(),
        },
        Spec::Choice(_) => v.clone(),
    }
}

pub struct Library {
    path: PathBuf,
    data: Mutex<Value>,
}

impl Library {
    pub fn open(path: PathBuf) -> Self {
        let mut data = read_json(&path).filter(|v| v.is_object()).unwrap_or_else(|| json!({}));
        let obj = data.as_object_mut().unwrap();
        obj.entry("version").or_insert(json!(1));
        obj.entry("default_mode").or_insert(json!(ASK));
        obj.entry("games").or_insert(json!({}));
        if !obj.get("token").is_some_and(|t| t.is_string()) {
            obj.insert("token".into(), json!(token(16)));
        }
        let lib = Library { path, data: Mutex::new(data) };
        lib.save(&lib.data.lock().unwrap());
        lib
    }

    fn save(&self, data: &Value) {
        if let Ok(bytes) = serde_json::to_vec_pretty(data) {
            let _ = write_atomic(&self.path, &bytes);
        }
    }

    fn update<R>(&self, f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
        let mut d = self.data.lock().unwrap();
        let r = f(d.as_object_mut().unwrap());
        self.save(&d);
        r
    }

    fn read<R>(&self, f: impl FnOnce(&Map<String, Value>) -> R) -> R {
        f(self.data.lock().unwrap().as_object().unwrap())
    }

    pub fn token(&self) -> String {
        self.read(|d| d["token"].as_str().unwrap_or_default().to_string())
    }

    pub fn get_raw(&self, key: &str) -> Value {
        self.read(|d| d.get(key).cloned().unwrap_or(Value::Null))
    }

    pub fn set_raw(&self, key: &str, v: Value) {
        self.update(|d| d.insert(key.into(), v));
    }

    // ---- modes -------------------------------------------------------------------------
    pub fn default_mode(&self) -> String {
        self.read(|d| d["default_mode"].as_str().unwrap_or(ASK).to_string())
    }

    pub fn set_default_mode(&self, mode: &str) -> Result<(), String> {
        if !MODES.contains(&mode) {
            return Err(format!("unknown mode {mode}"));
        }
        self.update(|d| d.insert("default_mode".into(), json!(mode)));
        Ok(())
    }

    /// The mode for a game; "ask" falls back to the default for new games.
    pub fn mode_for(&self, name: &str) -> String {
        let mode = self.read(|d| {
            d["games"].get(name.to_lowercase()).and_then(|g| g["mode"].as_str()).unwrap_or(ASK).to_string()
        });
        if mode == ASK { self.default_mode() } else { mode }
    }

    pub fn set_mode(&self, name: &str, mode: &str) -> Result<(), String> {
        if !MODES.contains(&mode) {
            return Err(format!("unknown mode {mode}"));
        }
        self.update(|d| {
            entry(d, name)["mode"] = json!(mode);
        });
        Ok(())
    }

    // ---- settings ------------------------------------------------------------------------
    pub fn prefs(&self) -> Map<String, Value> {
        let saved = self.read(|d| d.get("prefs").cloned().unwrap_or(json!({})));
        pref_specs()
            .into_iter()
            .map(|(k, spec, default)| {
                let v = saved.get(k).filter(|v| valid(&spec, v)).cloned().unwrap_or(default);
                (k.to_string(), v)
            })
            .collect()
    }

    pub fn set_pref(&self, key: &str, value: &Value) -> Result<(), String> {
        let (_, spec, _) = pref_specs().into_iter().find(|(k, _, _)| *k == key).ok_or(format!("unknown setting {key}"))?;
        let v = coerce(&spec, value);
        if !valid(&spec, &v) {
            return Err(format!("{key}: invalid value {value}"));
        }
        self.update(|d| {
            let prefs = d.entry("prefs").or_insert(json!({}));
            if !prefs.is_object() {
                *prefs = json!({});
            }
            prefs[key] = v;
        });
        Ok(())
    }

    pub fn reset_advanced(&self) {
        self.update(|d| {
            if let Some(p) = d.get_mut("prefs").and_then(|p| p.as_object_mut()) {
                p.retain(|k, _| !k.starts_with("adv_"));
            }
        });
    }

    // ---- games -----------------------------------------------------------------------------
    pub fn add(&self, name: &str, info: Value) {
        self.update(|d| merge_info(entry(d, name), &info));
    }

    pub fn remove(&self, name: &str) {
        self.update(|d| d["games"].as_object_mut().map(|g| g.remove(&name.to_lowercase())));
    }

    pub fn game_started(&self, name: &str, info: Value) {
        self.update(|d| {
            let g = entry(d, name);
            merge_info(g, &info);
            g["sessions"] = json!(g["sessions"].as_i64().unwrap_or(0) + 1);
            g["last_played"] = json!(unix_now() as i64);
        });
    }

    pub fn game_ended(&self, name: &str, seconds: f64) {
        self.update(|d| {
            if let Some(g) = d["games"].get_mut(name.to_lowercase()) {
                g["playtime"] = json!(g["playtime"].as_i64().unwrap_or(0) + seconds.max(0.0) as i64);
            }
        });
    }

    pub fn games(&self) -> Vec<Value> {
        self.read(|d| d["games"].as_object().map(|g| g.values().cloned().collect()).unwrap_or_default())
    }

    /// (exe, game name) pairs for every game in the library.
    pub fn custom_exes(&self) -> Vec<(String, String)> {
        self.games()
            .iter()
            .flat_map(|g| {
                let name = g["name"].as_str().unwrap_or_default().to_string();
                g["exes"].as_array().cloned().unwrap_or_default().into_iter()
                    .filter_map(move |e| e.as_str().map(|e| (e.to_string(), name.clone())))
            })
            .collect()
    }

    /// Add games found on the PC. Never changes the modes the user picked.
    pub fn merge_imported(&self, entries: &[Value]) -> usize {
        self.update(|d| {
            let mut added = 0;
            for e in entries {
                let Some(name) = e["name"].as_str() else { continue };
                let is_new = d["games"].get(name.to_lowercase()).is_none();
                let g = entry(d, name);
                if is_new {
                    added += 1;
                    g["source"] = e.get("source").cloned().unwrap_or(json!("custom"));
                    g.as_object_mut().unwrap().remove("added");
                }
                for k in ["discord_id", "icon", "steam_appid", "installed", "exe_path"] {
                    if e.get(k).is_some_and(|v| !v.is_null()) {
                        g[k] = e[k].clone();
                    }
                }
                union_exes(g, &e["exes"]);
                for k in ["last_played", "playtime"] {
                    if let Some(v) = e[k].as_i64().filter(|v| *v > 0) {
                        g[k] = json!(g[k].as_i64().unwrap_or(0).max(v));
                    }
                }
            }
            d.insert("last_scan".into(), json!(unix_now() as i64));
            added
        })
    }
}

fn entry<'a>(d: &'a mut Map<String, Value>, name: &str) -> &'a mut Value {
    let games = d.entry("games").or_insert(json!({}));
    games.as_object_mut().unwrap().entry(name.to_lowercase()).or_insert_with(|| {
        json!({"name": name, "mode": ASK, "exes": [], "source": "custom", "added": unix_now() as i64,
               "sessions": 0, "playtime": 0, "last_played": null})
    })
}

fn union_exes(g: &mut Value, exes: &Value) {
    let mut all: Vec<String> = g["exes"].as_array().cloned().unwrap_or_default().iter()
        .filter_map(|e| e.as_str().map(|s| s.to_lowercase())).collect();
    for e in exes.as_array().cloned().unwrap_or_default() {
        if let Some(s) = e.as_str() {
            all.push(s.to_lowercase());
        }
    }
    all.sort();
    all.dedup();
    g["exes"] = json!(all);
}

fn merge_info(g: &mut Value, info: &Value) {
    if let Some(obj) = info.as_object() {
        for (k, v) in obj {
            if v.is_null() {
                continue;
            }
            if k == "exes" {
                union_exes(g, v);
            } else {
                g[k] = v.clone();
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn tmp() -> (tempdir::Dir, Library) {
        let d = tempdir::Dir::new();
        let lib = Library::open(d.0.join("library.json"));
        (d, lib)
    }

    pub mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!("fragtape-test-{}", crate::util::token(6)));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn modes_and_default() {
        let (_d, lib) = tmp();
        assert_eq!(lib.mode_for("X"), ASK);
        lib.set_default_mode(FULL).unwrap();
        assert_eq!(lib.mode_for("X"), FULL);
        lib.set_mode("X", OFF).unwrap();
        assert_eq!(lib.mode_for("x"), OFF);
        assert!(lib.set_mode("X", "bogus").is_err());
    }

    #[test]
    fn prefs_validate_and_coerce() {
        let (_d, lib) = tmp();
        assert_eq!(lib.prefs()["fps"], json!(60));
        assert_eq!((lib.prefs()["notify_highlight"].clone(), lib.prefs()["notify_clip"].clone()), (json!(true), json!(true)));
        lib.set_pref("notify_clip", &json!(false)).unwrap();
        assert_eq!(lib.prefs()["notify_clip"], json!(false));
        lib.set_pref("fps", &json!("30")).unwrap();
        lib.set_pref("mic_mono", &json!("false")).unwrap();
        lib.set_pref("adv_bitrate", &json!(45000.0)).unwrap();
        assert!(lib.set_pref("adv_bitrate", &json!(5)).is_err());
        assert!(lib.set_pref("hotkey", &json!("F1")).is_err());
        assert!(lib.set_pref("nope", &json!(1)).is_err());
        let p = lib.prefs();
        assert_eq!((p["fps"].clone(), p["mic_mono"].clone(), p["adv_bitrate"].clone()), (json!(30), json!(false), json!(45000)));
        lib.reset_advanced();
        assert_eq!(lib.prefs()["adv_bitrate"], json!(30000));
        assert_eq!(lib.prefs()["fps"], json!(30));
    }

    #[test]
    fn invalid_saved_values_fall_back() {
        let d = tempdir::Dir::new();
        let path = d.0.join("library.json");
        std::fs::write(&path, r#"{"prefs": {"capture": "scene", "quality": "keep", "fps": 0, "audio": "game_mic"}}"#).unwrap();
        let p = Library::open(path).prefs();
        assert_eq!((p["capture"].clone(), p["quality"].clone(), p["fps"].clone(), p["audio"].clone()),
                   (json!("game"), json!("high"), json!(60), json!("game_mic")));
    }

    #[test]
    fn merge_keeps_modes() {
        let (_d, lib) = tmp();
        lib.set_mode("Apex Legends", FULL).unwrap();
        let n = lib.merge_imported(&[json!({"name": "Apex Legends", "source": "steam", "exes": ["r5apex.exe"], "playtime": 100}),
                                     json!({"name": "New Game", "source": "epic"})]);
        assert_eq!(n, 1);
        assert_eq!(lib.mode_for("apex legends"), FULL);
        assert!(lib.custom_exes().contains(&("r5apex.exe".into(), "Apex Legends".into())));
    }
}
