//! Turns Fragtape's state (the same JSON the HTML window used) into the native window's models.

use serde_json::{Map, Value};
use slint::{Color, Image, ModelRc, SharedString, VecModel};

use super::images::{Cache, Workers};
use super::{ClipCard, ClipGroup, Found, GameRow, Opt, SRow, Swatch, ThemeCard};

pub fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}
fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

// ---- small text helpers (same wording as before) --------------------------------------------
pub fn clock(s: f64) -> String {
    let s = s.max(0.0) as u64;
    let (h, m, x) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{x:02}") } else { format!("{m}:{x:02}") }
}
fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
pub fn ago(ts: f64) -> String {
    if ts <= 0.0 {
        return "never played".into();
    }
    let s = now() - ts;
    if s < 90.0 { return "just now".into(); }
    if s < 3600.0 { return format!("{} min ago", (s / 60.0).round()); }
    if s < 86400.0 { return format!("{} h ago", (s / 3600.0).round()); }
    let d = (s / 86400.0).round();
    if d == 1.0 { "yesterday".into() } else if d < 60.0 { format!("{d} days ago") } else { format!("{} months ago", (d / 30.0).round()) }
}
fn playtime(s: f64) -> Option<String> {
    if s <= 0.0 { return None; }
    let (h, m) = ((s / 3600.0) as u64, ((s % 3600.0) / 60.0).round() as u64);
    Some(if h > 0 { format!("{h} h {m} min") } else { format!("{m} min") })
}
pub fn size(b: f64) -> String {
    if b > 1e9 { format!("{:.1} GB", b / 1e9) } else { format!("{} MB", (b / 1e6).round().max(1.0)) }
}
fn letter(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or("?".into())
}
fn opts(list: &[(&str, &str)]) -> Vec<Opt> {
    list.iter().map(|(v, l)| Opt { value: ss(v), label: ss(l), disabled: false }).collect()
}
/// A preference as the text the controls compare with ("true", "60", "high"...).
fn cur(p: &Map<String, Value>, k: &str) -> String {
    match p.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

// ---- clips ----------------------------------------------------------------------------------
pub struct ClipFilter<'a> { pub kind: &'a str, pub game: &'a str, pub query: &'a str }

pub fn clip_groups(clips: &[Value], state: &Value, f: &ClipFilter, cache: &mut Cache, work: &Workers,
                   thumb: impl Fn(&str) -> Option<std::path::PathBuf>, path_of: impl Fn(&str) -> Option<std::path::PathBuf>) -> Vec<ClipGroup> {
    let q = f.query.trim().to_lowercase();
    let mut groups: Vec<(String, Vec<ClipCard>)> = vec![];
    for c in clips {
        let kind = c["kind"].as_str().unwrap_or("session");
        let fav = c["favorite"] == true;
        let game = c["game"].as_str().unwrap_or("Other");
        if !(f.kind == "all" || (f.kind == "fav" && fav) || f.kind == kind) || (!f.game.is_empty() && f.game != game) {
            continue;
        }
        let title = c["title"].as_str().unwrap_or("");
        if !q.is_empty() && !format!("{title} {game} {}", c["name"].as_str().unwrap_or("")).to_lowercase().contains(&q) {
            continue;
        }
        let id = c["id"].as_str().unwrap_or("");
        let live = c["live"] == true;
        let img = match thumb(id) {
            Some(p) => cache.get(&p),
            None => {
                if !live { if let Some(p) = path_of(id) { work.want_thumb(id, p); } }
                Image::default()
            }
        };
        if !live && c["duration"].is_null() && thumb(id).is_some() {
            if let Some(p) = path_of(id) { work.want_thumb(id, p); } // the duration is still missing
        }
        let card = ClipCard {
            id: ss(id),
            title: ss(title),
            sub: ss(format!("{} · {}", ago(c["mtime"].as_f64().unwrap_or(0.0)), size(c["size"].as_f64().unwrap_or(0.0)))),
            kind: ss(if live { "live" } else { kind }),
            duration: ss(c["duration"].as_f64().map(clock).unwrap_or_default()),
            moments: c["markers"].as_array().map(|m| m.len()).unwrap_or(0) as i32,
            favorite: fav,
            thumb: img,
        };
        match groups.iter_mut().find(|(g, _)| g == game) {
            Some((_, v)) => v.push(card),
            None => groups.push((game.to_string(), vec![card])),
        }
    }
    groups.into_iter().map(|(game, cards)| {
        let g = state["games"].as_array().and_then(|gs| gs.iter().find(|x| x["name"].as_str().map(|n| n.eq_ignore_ascii_case(&game)) == Some(true)));
        let icon = work.icon(&game, g, None).map(|p| cache.get(&p)).unwrap_or_default();
        ClipGroup { letter: ss(letter(&game)), icon, count: ss(format!("{} video{}", cards.len(), if cards.len() > 1 { "s" } else { "" })),
                    game: ss(&game), cards: model(cards) }
    }).collect()
}

pub fn clip_games(clips: &[Value]) -> Vec<Opt> {
    let mut g: Vec<String> = clips.iter().filter_map(|c| c["game"].as_str().map(String::from)).collect();
    g.sort();
    g.dedup();
    let mut out = vec![Opt { value: ss(""), label: ss("All games"), disabled: false }];
    out.extend(g.iter().map(|n| Opt { value: ss(n), label: ss(n), disabled: false }));
    out
}

// ---- games ----------------------------------------------------------------------------------
pub fn game_rows(state: &Value, filter: &str, view: &str, cache: &mut Cache, work: &Workers, discord: impl Fn(&Value) -> Option<String>) -> Vec<GameRow> {
    let playing = state["status"]["game"].as_str().unwrap_or("");
    let f = filter.trim().to_lowercase();
    let full = |name: &str| -> Option<Value> { None.or_else(|| Some(Value::String(name.into()))) };
    let _ = full;
    let mut games: Vec<&Value> = state["games"].as_array().map(|v| v.iter().collect()).unwrap_or_default();
    games.retain(|g| {
        let n = g["name"].as_str().unwrap_or("");
        (f.is_empty() || n.to_lowercase().contains(&f)) && (view == "all" || g["installed"] == true || n == playing)
    });
    let key = |g: &Value| g["last_played"].as_f64().or(g["added"].as_f64()).unwrap_or(0.0);
    games.sort_by(|a, b| {
        let (pa, pb) = (a["name"] == playing, b["name"] == playing);
        pb.cmp(&pa).then(key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal))
    });
    games.iter().map(|g| {
        let name = g["name"].as_str().unwrap_or("");
        let source = match g["source"].as_str().unwrap_or("") { "steam" => "Steam", "riot" => "Riot", "epic" => "Epic", "custom" => "Custom", _ => "" };
        let when = if name == playing { "playing now".to_string() } else { ago(g["last_played"].as_f64().unwrap_or(0.0)) };
        let meta: Vec<String> = [Some(source.to_string()).filter(|s| !s.is_empty()), Some(when), playtime(g["playtime"].as_f64().unwrap_or(0.0))]
            .into_iter().flatten().collect();
        let icon = work.icon(name, Some(g), discord(g)).map(|p| cache.get(&p)).unwrap_or_default();
        GameRow { name: ss(name), letter: ss(letter(name)), icon, meta: ss(meta.join(" · ")), mode: ss(g["mode"].as_str().unwrap_or("ask")),
                  kills: g["kills"] == true, playing: name == playing }
    }).collect()
}

pub fn found_rows(query: &str, results: &[Value], db_ready: bool, cache: &mut Cache, icon: impl Fn(&Value) -> Option<std::path::PathBuf>) -> (Vec<Found>, String) {
    let q = query.trim();
    let mut out = vec![];
    if q.len() < 2 {
        return (out, String::new());
    }
    if q.to_lowercase().ends_with(".exe") || !db_ready {
        let exe = if q.to_lowercase().ends_with(".exe") { q.to_string() } else { format!("{q}.exe") };
        out.push(Found { id: ss(""), name: ss(format!("Custom game: {exe}")), letter: ss("?"), icon: Image::default(), kills: false, added: false, exe: ss(exe) });
    }
    for r in results {
        let name = r["name"].as_str().unwrap_or("");
        out.push(Found { id: ss(r["id"].as_str().unwrap_or("")), name: ss(name), letter: ss(letter(name)),
                         icon: icon(r).map(|p| cache.get(&p)).unwrap_or_default(), kills: r["kills"] == true, added: r["added"] == true, exe: ss("") });
    }
    let note = if out.is_empty() { "No game found. Type its .exe name to add it.".to_string() } else { String::new() };
    (out, note)
}

// ---- settings ---------------------------------------------------------------------------------
fn row(kind: &str, key: &str, label: &str, why: &str) -> SRow {
    SRow { kind: ss(kind), key: ss(key), label: ss(label), why: ss(why), options: model(vec![]), current: ss(""), value: 0.0, min: 0.0, max: 1.0,
           text: ss(""), dim: false }
}
fn seg(p: &Map<String, Value>, key: &str, label: &str, options: &[(&str, &str)], why: &dyn Fn(&str) -> String) -> SRow {
    let c = cur(p, key);
    SRow { options: model(opts(options)), current: ss(&c), why: ss(why(&c)), ..row("seg", key, label, "") }
}
fn seg_disabled(p: &Map<String, Value>, key: &str, label: &str, options: &[(&str, &str)], off: &[&str], why: String) -> SRow {
    let mut o = opts(options);
    for x in o.iter_mut() { x.disabled = off.contains(&x.value.as_str()); }
    SRow { options: model(o), current: ss(cur(p, key)), ..row("seg", key, label, &why) }
}
fn fixed(why: &'static str) -> impl Fn(&str) -> String {
    move |_| why.to_string()
}
fn slider(p: &Map<String, Value>, key: &str, label: &str, min: f32, max: f32, text: String, why: &str) -> SRow {
    SRow { value: p.get(key).and_then(|v| v.as_f64()).unwrap_or(min as f64) as f32, min, max, text: ss(text), ..row("slider", key, label, why) }
}
fn number(p: &Map<String, Value>, key: &str, label: &str, unit: &str, why: &str) -> SRow {
    SRow { text: ss(cur(p, key)), options: model(opts(&[("", unit)])), ..row("number", key, label, why) }
}
fn buttons(key: &str, label: &str, text: &str, why: &str, btns: &[(&str, &str, bool)]) -> SRow {
    let o = btns.iter().map(|(v, l, primary)| Opt { value: ss(v), label: ss(l), disabled: *primary }).collect();
    SRow { text: ss(text), options: model(o), ..row("buttons", key, label, why) }
}
fn per_hour(kbps: f64) -> String {
    let gb = kbps * 3600.0 / 8.0 / 1e6;
    if gb >= 1.0 { format!("about {gb:.1} GB per hour") } else { format!("about {} MB per hour", (gb * 1000.0).round()) }
}

pub fn settings_rows(tab: &str, state: &Value, gsi_note: &str) -> Vec<SRow> {
    let empty = Map::new();
    let p = state["prefs"].as_object().unwrap_or(&empty);
    let a = &state["app"];
    let onoff = [("true", "On"), ("false", "Off")];
    let mut r = vec![];
    match tab {
        "general" => {
            r.push(seg(p, "autostart", "Start with Windows", &onoff, &fixed("Starts in the tray, so games are recorded without opening anything.")));
            r.push(seg(p, "hotkey", "Highlight hotkey", &[("F8", "F8"), ("F9", "F9"), ("F10", "F10"), ("Ctrl+F8", "Ctrl+F8"), ("off", "Off")],
                       &fixed("Press it in game to save a clip (Clips mode) or mark the moment (Record mode).")));
            r.push(seg(p, "rec_hotkey", "Screen recording hotkey", &[("Ctrl+F9", "Ctrl+F9"), ("Ctrl+F10", "Ctrl+F10"), ("Ctrl+F11", "Ctrl+F11"), ("off", "Off")],
                       &fixed("Starts and stops recording your whole screen, any time. Saved to Screen recordings.")));
            r.push(seg(p, "notify_highlight", "Highlight notification", &onoff, &|v| if v == "true" { "A small card in the corner when you press the hotkey.".into() } else { "No card when you press the hotkey.".into() }));
            r.push(seg(p, "notify_clip", "Clip saved notification", &onoff, &|v| if v == "true" { "A small card in the corner when a clip is saved.".into() } else { "Clips are saved silently.".into() }));
            let o = &a["obs"];
            let found = a["obs_found"] != false;
            let mut text = match o["path"].as_str() { Some(pth) if found => format!("{pth}{}", if o["custom"].is_null() { "  (found automatically)" } else { "" }), _ => "Not installed".into() };
            if !found && o["note"].as_str().is_some_and(|n| !n.is_empty()) { text = format!("Not installed. {}", o["note"].as_str().unwrap()); }
            let mut b: Vec<(&str, &str, bool)> = vec![];
            if !found { b.push(if o["installing"] == true { ("", "Installing…", false) } else { ("install", "Install OBS Studio", true) }); }
            b.push(("browse", if found { "Change…" } else { "Locate…" }, false));
            if !o["custom"].is_null() { b.push(("auto", "Auto-detect", false)); }
            r.push(buttons("obs", "OBS Studio", &text, "The recording engine. Found automatically; pick a folder only for a portable or unusual install.", &b));
        }
        "recording" => {
            r.push(seg(p, "capture", "Capture", &[("game", "Game window"), ("screen", "Whole screen")], &|v| if v == "screen" {
                "The whole monitor: use this if a game's anti-cheat blocks game capture (black video).".into() } else { "Only the game, nothing else on your screen.".into() }));
            r.push(seg(p, "cursor", "Record cursor", &onoff, &|v| if v == "true" { "Your mouse pointer shows in the video (only when the game shows it).".into() } else { "No mouse pointer in the video.".into() }));
            r.push(seg(p, "audio", "Audio", &[("game", "Game"), ("game_mic", "Game + mic"), ("all", "All PC sound")], &|v| match v {
                "game_mic" => "The game's sound and your microphone.".into(), "all" => "Everything you hear, plus your microphone.".into(),
                _ => "Only the game's sound: no Discord, music or notifications.".into() }));
            r.push(seg(p, "mic_mono", "Microphone in mono", &onoff, &|v| if v == "true" { "Your voice in both ears (for single-input mics/interfaces).".into() } else { "Keep the mic as stereo.".into() }));
            r.push(seg(p, "mic_boost", "Microphone boost", &[("0", "Off"), ("6", "+6 dB"), ("12", "+12 dB"), ("18", "+18 dB"), ("24", "+24 dB")], &|v| match v {
                "0" => "The microphone as Windows gives it.".into(),
                _ => "Makes your voice louder in recordings (a limiter stops it from distorting). Applies from the next game.".into() }));
            r.push(seg(p, "quality", "Quality", &[("small", "Small"), ("balanced", "Balanced"), ("high", "High"), ("max", "Max"), ("custom", "Custom")], &|v| match v {
                "small" => "Smaller files.".into(), "balanced" => "Good quality, moderate size.".into(), "max" => "Best quality, big files.".into(),
                "custom" => "Uses the encoder settings on the Advanced tab.".into(), _ => "Great quality (recommended).".into() }));
            r.push(seg(p, "fps", "Frame rate", &[("30", "30"), ("60", "60")], &|v| if v == "30" { "30 fps, smaller files.".into() } else { "60 fps, smooth (recommended for games).".into() }));
            r.push(seg(p, "clip_length", "Clip length", &[("15", "15 s"), ("30", "30 s"), ("60", "60 s"), ("120", "2 min")], &fixed("How much is kept before a highlight in Clips mode.")));
        }
        "advanced" => advanced(p, a, &mut r),
        "appearance" => {
            r.push(row("themes", "theme", "", ""));
            r.push(row("swatches", "accent", "Accent colour", "Buttons, selection and the in-game cards. The rainbow one uses the theme's colour."));
            r.push(seg(p, "text_size", "Text size", &[("small", "Small"), ("normal", "Normal"), ("large", "Large")], &fixed("Makes the text in the window smaller or bigger.")));
            r.push(seg(p, "thumb_size", "Clip thumbnails", &[("small", "Small"), ("medium", "Medium"), ("large", "Large")], &fixed("How big the videos are in Clips.")));
            r.push(seg(p, "animations", "Animations", &onoff, &fixed("Off: nothing slides, fades or pulses.")));
            r.push(seg(p, "card_corner", "In-game cards", &[("top_left", "Top left"), ("top_right", "Top right"), ("bottom_left", "Bottom left"), ("bottom_right", "Bottom right")],
                       &fixed("Which corner of the game window the cards use.")));
            r.push(seg(p, "card_time", "Card time", &[("short", "2 s"), ("normal", "3.5 s"), ("long", "6 s")], &fixed("How long highlight and clip cards stay. The new-game question stays 30 s.")));
            r.push(buttons("test-card", "", "", "Shows a card in the corner of this screen.", &[("show", "Show a test card", false)]));
        }
        "storage" => {
            r.push(buttons("folder", "Videos folder", a["folder"].as_str().unwrap_or(""), "", &[("open", "Open", false)]));
            let ff = a["ffmpeg"] == true;
            let (text, b): (&str, Vec<(&str, &str, bool)>) = if ff { ("ffmpeg found ✓", vec![]) }
                else if a["installing_ffmpeg"] == true { ("Installing ffmpeg…", vec![]) } else { ("", vec![("install", "Install ffmpeg", false)]) };
            r.push(buttons("ffmpeg", "Clip editor", text, "Saving a part of a video as a new clip uses ffmpeg (free).", &b));
        }
        "kills" => {
            r.push(SRow { text: ss("Automatic"), ..row("text", "", "League of Legends", "Read from the game's own local API, nothing to set up.") });
            r.push(buttons("gsi", "CS2 / Dota 2", "", if gsi_note.is_empty() {
                "Adds Valve's Game State Integration file to the game folders (restart the game afterwards). Dota 2 also needs the launch option -gamestateintegration." } else { gsi_note },
                &[("install", "Set up", false)]));
            r.push(SRow { text: ss("Hotkey"), ..row("text", "", "Every other game", "Press your highlight hotkey to mark or clip a moment.") });
        }
        _ => {
            r.push(SRow { text: ss(format!("Fragtape v{}", a["version"].as_str().unwrap_or(""))), ..row("heading", "", "How it works", "") });
            r.push(SRow { text: ss("Fragtape is a small Rust app. It starts its own hidden copy of OBS Studio only while a game is being recorded, with its own settings: your normal OBS is never touched. Between games only the game detection runs (about 2 ms every 2 seconds). This window can be closed: Fragtape keeps running in the tray."),
                          ..row("para", "", "", "") });
            r.push(row("heading", "", "Activity", ""));
            let log: Vec<String> = a["log"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            r.push(SRow { text: ss(log.join("\n")), ..row("log", "", "", "") });
        }
    }
    r
}

fn advanced(p: &Map<String, Value>, a: &Value, r: &mut Vec<SRow>) {
    let custom = p.get("quality").and_then(|v| v.as_str()) == Some("custom");
    let vendor = a["vendor"].as_str().unwrap_or("");
    let avail: Vec<&str> = a["encoders"].as_array().map(|e| e.iter().filter_map(|x| x["id"].as_str()).collect()).unwrap_or_default();
    let ids: &[(&str, &str)] = match vendor {
        "Nvidia" => &[("hevc", "obs_nvenc_hevc_tex"), ("av1", "obs_nvenc_av1_tex")],
        "Amd" => &[("hevc", "h265_texture_amf"), ("av1", "av1_texture_amf")],
        "Intel" => &[("hevc", "obs_qsv11_hevc"), ("av1", "obs_qsv11_av1")],
        _ => &[],
    };
    let is_cpu = cur(p, "adv_encoder") == "x264" || vendor == "Cpu";
    let missing: Vec<&str> = ["hevc", "av1"].into_iter().filter(|c| {
        is_cpu || match ids.iter().find(|(k, _)| k == c) { Some((_, id)) => !avail.is_empty() && !avail.contains(id), None => true }
    }).collect();
    let mut note = SRow { label: ss(if custom { "Quality is set to Custom: the encoder settings below are used." }
        else { "The encoder settings below are only used with Quality = Custom. Output, audio and performance settings always apply." }), ..row("note", "advnote", "", "") };
    note.options = model(if custom { vec![] } else { opts(&[("custom", "Use custom")]) });
    r.push(note);
    let enc_note = if vendor == "Cpu" { "no GPU encoder found: x264 (CPU)".to_string() } else { format!("{vendor} GPU{}", if avail.is_empty() { " · codecs are confirmed after the first recording" } else { "" }) };
    r.push(SRow { why: ss(enc_note), ..row("heading", "", "Encoder", "") });
    let dim = !custom;
    let mut enc = vec![];
    let codec = cur(p, "adv_codec");
    enc.push(seg_disabled(p, "adv_codec", "Codec", &[("h264", "H.264"), ("hevc", "HEVC (H.265)"), ("av1", "AV1")], &missing, match codec.as_str() {
        "hevc" => "About 30% smaller at the same quality; not every site or player supports it.", "av1" => "Smallest files; needs an RTX 40 / RX 7000 / Arc GPU or newer.",
        _ => "Plays everywhere (recommended for sharing)." }.into()));
    enc.push(seg(p, "adv_encoder", "Encoder", &[("auto", "GPU"), ("x264", "CPU (x264)")], &|v| if v == "x264" {
        "Uses your processor: can cost FPS in games. H.264 only.".into() } else { "Your graphics card's encoder: almost no FPS cost.".into() }));
    let rc = cur(p, "adv_rate_control");
    enc.push(seg(p, "adv_rate_control", "Rate control", &[("cqp", "Constant quality"), ("vbr", "Variable bitrate"), ("cbr", "Constant bitrate")], &|v| match v {
        "vbr" => "Aims for the bitrate, can go up to the maximum on busy scenes.".into(), "cbr" => "Fixed bitrate: predictable file size.".into(),
        _ => "Same quality everywhere; busy scenes get bigger. Best for recording.".into() }));
    if rc == "cqp" {
        let v = p.get("adv_cq").and_then(|v| v.as_i64()).unwrap_or(20);
        let word = if v <= 16 { "near lossless" } else if v <= 22 { "great" } else if v <= 28 { "good" } else { "small" };
        enc.push(slider(p, "adv_cq", if is_cpu { "Quality (CRF)" } else { "Quality level (CQ)" }, 1.0, 51.0, format!("{v} · {word}"), "Lower = better quality and bigger files. 18–23 is a good range."));
    } else {
        let br = p.get("adv_bitrate").and_then(|v| v.as_f64()).unwrap_or(30000.0);
        enc.push(number(p, "adv_bitrate", "Bitrate", "kbps", &format!("{}. 1080p60: 20,000–40,000 · 1440p60: 40,000–60,000 · 4K60: 80,000+", per_hour(br))));
        if rc == "vbr" { enc.push(number(p, "adv_max_bitrate", "Max bitrate", "kbps", "Upper limit on busy scenes (never below the bitrate).")); }
    }
    let preset = p.get("adv_preset").and_then(|v| v.as_i64()).unwrap_or(5).clamp(1, 7) as usize;
    enc.push(slider(p, "adv_preset", "Encoder preset", 1.0, 7.0, ["Fastest", "Faster", "Fast", "Medium", "Slow", "Slower", "Best quality"][preset - 1].into(),
                    "Slower = better quality per bit, more encoder work (GPU encoders handle Slower fine)."));
    enc.push(number(p, "adv_keyint", "Keyframe interval", "seconds", "How often a full frame is stored. 2 s is standard; 1 s makes trimming more precise."));
    enc.push(seg(p, "adv_bframes", "B-frames", &[("0", "0"), ("1", "1"), ("2", "2"), ("3", "3"), ("4", "4")], &fixed("More = smaller files; 2 is a good default.")));
    if !is_cpu && vendor == "Nvidia" {
        enc.push(seg(p, "adv_lookahead", "Look-ahead", &[("true", "On"), ("false", "Off")], &fixed("Better quality in fast scenes, slightly more GPU use.")));
    }
    enc.push(seg_disabled(p, "adv_color", "Color depth", &[("nv12", "8-bit"), ("p010", "10-bit")], if codec == "h264" || is_cpu { &["p010"] } else { &[] },
                          "10-bit: smoother gradients; HEVC/AV1 only.".into()));
    for mut e in enc { e.dim = dim; r.push(e); }

    let n = &a["next"];
    r.push(row("heading", "", "Output", ""));
    r.push(seg(p, "adv_resolution", "Resolution", &[("0", "Native"), ("2160", "2160p"), ("1440", "1440p"), ("1080", "1080p"), ("900", "900p"), ("720", "720p")],
               &|_| format!("Monitor {} → recording {}. Never upscales.", n["base"].as_str().unwrap_or("").replace('x', "×"), n["out"].as_str().unwrap_or("").replace('x', "×"))));
    r.push(seg(p, "adv_scale_filter", "Scale filter", &[("lanczos", "Lanczos"), ("bicubic", "Bicubic"), ("bilinear", "Bilinear"), ("area", "Area")], &fixed("Used when the resolution is lowered. Lanczos is the sharpest.")));
    let basic = format!("Basic ({} fps)", cur(p, "fps"));
    r.push(seg(p, "adv_fps", "Frame rate", &[("0", basic.as_str()), ("30", "30"), ("60", "60"), ("120", "120"), ("144", "144"), ("165", "165"), ("240", "240")],
               &fixed("Above 60 fps needs a fast encoder and makes bigger files.")));
    r.push(seg(p, "adv_format", "File format", &[("hybrid_mp4", "Hybrid MP4"), ("hybrid_mov", "Hybrid MOV"), ("mkv", "MKV"), ("mp4", "MP4")], &|v| match v {
        "hybrid_mov" => "Like Hybrid MP4, in a MOV container.".into(), "mkv" => "Survives crashes, but the Fragtape player can't play it and there are no chapters.".into(),
        "mp4" => "Classic MP4: a crash during recording loses the file.".into(), _ => "Recommended: survives crashes, plays everywhere, kills become chapters.".into() }));
    r.push(row("heading", "", "Audio", ""));
    r.push(seg(p, "adv_audio_bitrate", "Audio bitrate", &[("96", "96"), ("128", "128"), ("160", "160"), ("192", "192"), ("256", "256"), ("320", "320")], &fixed("kbps (AAC). 192 is plenty for game audio.")));
    r.push(seg(p, "adv_sample_rate", "Sample rate", &[("44100", "44.1 kHz"), ("48000", "48 kHz")], &fixed("48 kHz is standard for video.")));
    r.push(seg(p, "adv_tracks", "Audio tracks", &[("single", "One mix"), ("separate", "Separate tracks")], &|v| if v == "separate" {
        "Track 1 = everything, 2 = game, 3 = microphone, 4 = desktop, for editing later (the player plays track 1).".into() } else { "Everything mixed into one track.".into() }));
    r.push(row("heading", "", "Performance", ""));
    r.push(seg(p, "adv_replay_mb", "Replay buffer memory", &[("512", "512 MB"), ("1024", "1 GB"), ("2048", "2 GB"), ("4096", "4 GB"), ("8192", "8 GB")], &fixed("RAM limit for Clips mode. High bitrates with long clips need more.")));
    r.push(seg(p, "adv_priority", "OBS priority", &[("normal", "Normal"), ("above_normal", "Above normal"), ("high", "High")], &fixed("Above normal avoids dropped frames when the game uses all the CPU.")));
    r.push(buttons("advreset", "", "", "", &[("reset", "Reset advanced settings", false)]));
}

pub fn themes() -> Vec<ThemeCard> {
    [("sharp", "Sharp minimal", "default"), ("win10dark", "Graphite", ""), ("metro", "Blocks", ""), ("win10light", "Paper", ""), ("vista", "Vista Aero", "")]
        .iter().map(|(id, name, sub)| { let (l, r) = super::theme::preview(id); ThemeCard { id: ss(id), name: ss(name), sub: ss(sub), left: l, right: r } }).collect()
}

pub fn swatches() -> Vec<Swatch> {
    ["theme", "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"].iter()
        .map(|id| Swatch { id: ss(id), color: super::theme::accent_swatch(id), title: ss(id) }).collect()
}

/// The colour of a moment icon.
pub fn moment_color(kind: &str, accent: Color) -> Color {
    let c = |h: u32| Color::from_rgb_u8((h >> 16) as u8, (h >> 8) as u8, h as u8);
    match kind {
        "first_blood" => c(0xf87171),
        "double_kill" | "triple_kill" | "quad_kill" | "penta_kill" => c(0xef4444),
        "ace" | "mvp" => c(0xfacc15),
        "objective" | "objective_steal" => c(0x38bdf8),
        "structure" => c(0x7dd3fc),
        "manual" => accent,
        "death" | "assist" => c(0x9ca3af),
        _ => c(0xf59e0b),
    }
}

/// Values from settings controls are text; turn them back into what the setting stores.
pub fn pref_value(key: &str, v: &str) -> Value {
    if v == "true" || v == "false" {
        return Value::Bool(v == "true");
    }
    let numeric = ["fps", "clip_length", "mic_boost", "adv_bitrate", "adv_max_bitrate", "adv_cq", "adv_preset", "adv_keyint", "adv_bframes",
                   "adv_resolution", "adv_fps", "adv_audio_bitrate", "adv_sample_rate", "adv_replay_mb"];
    if numeric.contains(&key) {
        if let Ok(n) = v.trim().replace(',', "").parse::<f64>() {
            return Value::from(n.round() as i64);
        }
    }
    Value::String(v.to_string())
}
