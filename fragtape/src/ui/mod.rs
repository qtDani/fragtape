//! The native window (Slint). Runs on the main thread, next to the tray icon.
//! It reads the same state as the old HTML window and calls the same actions (server::post).

mod data;
mod images;
mod player;
mod theme;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use slint::{ComponentHandle, SharedString, Timer, TimerMode};

use crate::app::Shared;
use data::model;

slint::include_modules!();

struct Ui {
    win: AppWindow,
    s: Arc<Shared>,
    work: Arc<images::Workers>,
    cache: images::Cache,
    state: Value,
    clips: Vec<Value>,
    keys: (String, String, String),   // what the models were built from (state, clips, filters)
    player: Option<(player::Player, Value)>,
    gsi_note: String,
    search: (String, Vec<Value>),
    toast_timer: Timer,
    _refresh: Timer,
    frames: Timer,
    watch: Timer,
    view: (bool, bool, u32, u32),   // minimized, in front, size: when it changes, repaint everything
}

thread_local! {
    static UI: RefCell<Option<Rc<RefCell<Ui>>>> = const { RefCell::new(None) };
    static WORK: RefCell<Option<Arc<images::Workers>>> = const { RefCell::new(None) };
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

/// Start the window system on the main thread (before other threads ask for the window).
pub fn init() {
    let _ = slint::invoke_from_event_loop(|| {});
}

/// Run the window system on this (the main) thread until `quit`.
pub fn run_loop() {
    let _ = slint::run_event_loop_until_quit();
}
pub fn quit() {
    let _ = slint::invoke_from_event_loop(|| { let _ = slint::quit_event_loop(); });
}

/// Show the window (from any thread).
pub fn show(s: Arc<Shared>) {
    let _ = slint::invoke_from_event_loop(move || open(s));
}

fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let ui = UI.with(|u| u.borrow().clone())?;
    let mut b = ui.try_borrow_mut().ok()?;
    Some(f(&mut b))
}

fn open(s: Arc<Shared>) {
    if let Some(()) = with(|u| { let _ = u.win.show(); bring_to_front(&u.win); full_repaint(&u.win); }) {
        return;
    }
    let win = match AppWindow::new() {
        Ok(w) => w,
        Err(e) => return crate::logf!("[ui] could not create the window: {e}"),
    };
    let work = WORK.with(|w| w.borrow_mut().get_or_insert_with(|| Arc::new(images::Workers::start(s.clone()))).clone());
    // the bomb as the window icon
    let px = crate::logo::rgba(64, (242, 242, 242), (255, 138, 28), Some((22, 24, 28)));
    let flat: Vec<u8> = px.iter().flatten().copied().collect();
    win.set_app_icon(slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&flat, 64, 64)));
    if let Ok(tab) = std::fs::read_to_string(crate::util::local_dir().join("window-tab.txt")) {
        win.global::<SettingsState>().set_tab(ss(tab.trim()));
    }
    let ui = Rc::new(RefCell::new(Ui {
        win: win.clone_strong(), s: s.clone(), work, cache: Default::default(), state: Value::Null, clips: vec![], keys: Default::default(),
        player: None, gsi_note: String::new(), search: Default::default(), toast_timer: Timer::default(), _refresh: Timer::default(), frames: Timer::default(),
        watch: Timer::default(), view: Default::default(),
    }));
    UI.with(|u| *u.borrow_mut() = Some(ui.clone()));
    wire(&win, &s);
    refresh(true);
    // refresh every second; frames every ~16 ms while the player is open
    ui.borrow()._refresh.start(TimerMode::Repeated, Duration::from_millis(1000), || refresh(false));
    ui.borrow().watch.start(TimerMode::Repeated, Duration::from_millis(200), watch_view);
    win.window().on_close_requested(|| {
        // close = free everything; Fragtape keeps running in the tray
        let _ = slint::invoke_from_event_loop(|| {
            if let Some(ui) = UI.with(|u| u.borrow_mut().take()) {
                let mut u = ui.borrow_mut();
                u.player = None;
                let _ = u.win.hide();
            }
        });
        slint::CloseRequestResponse::HideWindow
    });
    let _ = win.show();
    bring_to_front(&win);
    // the title-bar and taskbar icon, once the window really exists
    Timer::single_shot(Duration::from_millis(150), set_icons);
}

fn set_icons() {
    if let Some(w) = crate::win::main_window(&[std::process::id()]) {
        crate::win::set_window_icons(w.hwnd, crate::tray::app_hicon(16) as isize, crate::tray::app_hicon(32) as isize);
    }
}

fn bring_to_front(win: &AppWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let w = win.window();
    if w.is_minimized() { w.set_minimized(false); }
    if let Ok(h) = w.window_handle().window_handle() {
        if let RawWindowHandle::Win32(h) = h.as_raw() {
            crate::win::bring_to_front(h.hwnd.get() as _);
        }
    }
}

/// The software renderer only redraws what changed and reuses the last frame for the rest.
/// Windows sometimes throws that frame away (minimize, restore, covered, sleep, display change),
/// which left only bits of the window drawn over the desktop. So whenever the window's state
/// changes, redraw all of it. Flipping an invisible full-window overlay marks everything dirty.
fn full_repaint(win: &AppWindow) {
    let a = win.global::<AppState>();
    a.set_repaint(!a.get_repaint());
}

fn watch_view() {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    with(|u| {
        let w = u.win.window();
        let front = match w.window_handle().window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => crate::win::is_foreground(h.hwnd.get() as _),
            _ => false,
        };
        let s = w.size();
        let v = (w.is_minimized(), front, s.width, s.height);
        if v != u.view {
            u.view = v;
            full_repaint(&u.win);
            // and once more a moment later, after Windows has finished restoring/resizing
            let ww = u.win.as_weak();
            Timer::single_shot(Duration::from_millis(300), move || { if let Some(w) = ww.upgrade() { full_repaint(&w); } });
        }
    });
}

// ---- refresh -------------------------------------------------------------------------------------
fn refresh(force: bool) {
    let Some(ui) = UI.with(|u| u.borrow().clone()) else { return };
    let mut u = match ui.try_borrow_mut() { Ok(u) => u, Err(_) => return };
    let s = u.s.clone();
    let state = crate::app::state(&s);
    let clips = s.clips.list(s.active_path.lock().unwrap().as_deref());
    let dirty = u.work.dirty.swap(false, Ordering::Relaxed);
    let win = u.win.clone_strong();
    // theme first, so new models use the right colours
    if let Some(p) = state["prefs"].as_object() { theme::apply(&win, p); }
    live(&win, &state);
    let skey = serde_json::to_string(&(&state["games"], &state["prefs"], &state["db"], &state["scanning"], &state["default_mode"], &state["app"]["obs"],
                                       &state["app"]["ffmpeg"], &state["app"]["installing_ffmpeg"], &state["app"]["next"], &state["app"]["encoders"], &state["status"]["game"])).unwrap_or_default();
    let ckey = serde_json::to_string(&clips).unwrap_or_default();
    let cs = win.global::<ClipsState>();
    let fkey = format!("{}|{}|{}", cs.get_kind(), cs.get_game(), cs.get_query());
    let changed_state = force || dirty || skey != u.keys.0;
    let changed_clips = force || dirty || ckey != u.keys.1 || fkey != u.keys.2 || changed_state;
    let log_tab = win.global::<SettingsState>().get_tab() == "about";
    u.state = state.clone();
    if changed_clips {
        u.clips = clips.clone();
        let f = data::ClipFilter { kind: &cs.get_kind(), game: &cs.get_game(), query: &cs.get_query() };
        let (cl, st) = (u.clips.clone(), state.clone());
        let work = u.work.clone();
        let store = s.clips.clone();
        let store2 = s.clips.clone();
        let groups = data::clip_groups(&cl, &st, &f, &mut u.cache, &work, |id| store.thumb_path(id), |id| store2.path_for(id));
        cs.set_groups(model(groups));
        cs.set_games(model(data::clip_games(&cl)));
        let any = !cl.is_empty();
        cs.set_empty_title(ss(if any { "Nothing matches" } else { "No tapes yet" }));
        cs.set_empty_text(ss(if any { "Try another filter." } else { "Play a game: Fragtape records it (or just its highlights) and everything shows up here." }));
        win.global::<AppState>().set_clips_count(ss(if any { cl.len().to_string() } else { String::new() }));
        u.keys.1 = ckey;
        u.keys.2 = fkey;
    }
    if changed_state {
        games(&mut u, &win, &state);
        u.keys.0 = skey;
    }
    if changed_state || log_tab {
        settings(&u, &win, &state);
    }
}

fn live(win: &AppWindow, state: &Value) {
    let a = win.global::<AppState>();
    let st = &state["status"];
    let hotkey = state["prefs"]["hotkey"].as_str().unwrap_or("F8");
    let game = st["game"].as_str();
    let (rec, rep) = (st["recording"] == true, st["replay"] == true);
    let screen = st["screen"] == true;
    let key = state["prefs"]["rec_hotkey"].as_str().unwrap_or("off");
    a.set_can_screen(screen || !rec);
    a.set_screen_on(screen);
    a.set_screen_key(ss(if key == "off" { "" } else { key }));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    if screen {
        a.set_live_state(ss("rec"));
        a.set_live_title(ss("Screen recording"));
        a.set_live_sub(ss(match game { Some(g) => format!("Saved to Screen recordings · {g} is running"), None => "Saved to Screen recordings".into() }));
        a.set_live_timer(ss(data::clock(now - st["screen_since"].as_f64().unwrap_or(now))));
        a.set_can_mark(true);
        a.set_mark_text(ss(format!("Mark this moment ({hotkey})")));
        a.set_can_stop(false);
    } else {
        live_game(&a, st, hotkey, game, rec, rep, now);
    }
    a.set_asking(ss(st["asking"].as_array().and_then(|x| x.first()).and_then(|g| g.as_str()).unwrap_or("")));
    let o = &state["app"]["obs"];
    a.set_obs_missing(state["app"]["obs_found"] == false);
    a.set_obs_installing(o["installing"] == true);
    a.set_obs_note(ss(o["note"].as_str().unwrap_or("")));
}

fn live_game(a: &AppState, st: &Value, hotkey: &str, game: Option<&str>, rec: bool, rep: bool, now: f64) {
    a.set_live_state(ss(if rec { "rec" } else if rep { "clip" } else if game.is_some() { "watching" } else { "" }));
    a.set_live_title(ss(game.unwrap_or("Watching for games")));
    a.set_live_sub(ss(match game {
        Some(_) if rec => "Recording".to_string(),
        Some(_) if rep => format!("Highlights on · press {hotkey} to clip"),
        Some(_) => match st["mode"].as_str() { Some("full") => "Recording the whole session", Some("highlights") => "Saving highlights", Some("off") => "Not recording", _ => "Waiting for your choice" }.into(),
        None => "Start a game and Fragtape takes it from there".into(),
    }));
    let since = st["since"].as_f64().unwrap_or(0.0);
    a.set_live_timer(ss(if game.is_some() && since > 0.0 { data::clock(now - since) } else { String::new() }));
    a.set_can_mark(game.is_some() && (rec || rep));
    a.set_mark_text(ss(format!("Save highlight ({hotkey})")));
    a.set_can_stop(rec);
}

fn games(u: &mut Ui, win: &AppWindow, state: &Value) {
    let g = win.global::<GamesState>();
    let s = u.s.clone();
    let work = u.work.clone();
    let rows = data::game_rows(state, &g.get_filter(), &g.get_view(), &mut u.cache, &work, |game| crate::app::discord_icon_url(&s, game));
    g.set_games(model(rows));
    let total = state["games"].as_array().map(|x| x.len()).unwrap_or(0);
    g.set_total(total as i32);
    win.global::<AppState>().set_games_count(ss(if total > 0 { total.to_string() } else { String::new() }));
    g.set_default_mode(ss(state["default_mode"].as_str().unwrap_or("ask")));
    let ready = state["db"]["ready"] == true;
    g.set_db_ready(ready);
    g.set_db_text(ss(if ready { format!("{} games", state["db"]["count"].as_u64().unwrap_or(0)) } else { "downloading game list…".into() }));
    g.set_scanning(state["scanning"] == true);
}

fn settings(u: &Ui, win: &AppWindow, state: &Value) {
    let st = win.global::<SettingsState>();
    let tab = st.get_tab().to_string();
    st.set_rows(model(data::settings_rows(&tab, state, &u.gsi_note)));
    st.set_themes(model(data::themes()));
    st.set_swatches(model(data::swatches()));
    st.set_theme(ss(state["prefs"]["theme"].as_str().unwrap_or("sharp")));
    st.set_accent(ss(state["prefs"]["accent"].as_str().unwrap_or("theme")));
    let n = &state["app"]["next"];
    st.set_recinfo(ss(match n["out"].as_str() {
        Some(out) => format!("Next recording: {} · {} fps · {}", out.replace('x', "×"), n["fps"], n["encoder"].as_str().unwrap_or("")),
        None => String::new(),
    }));
}

// ---- actions ---------------------------------------------------------------------------------------
fn toast(msg: impl Into<String>, error: bool) {
    let msg = msg.into();
    with(|u| {
        let a = u.win.global::<AppState>();
        a.set_toast(ss(&msg));
        a.set_toast_error(error);
        u.toast_timer.start(TimerMode::SingleShot, Duration::from_millis(3500), || {
            with(|u| u.win.global::<AppState>().set_toast(ss("")));
        });
    });
}

/// Run an action like the old window's buttons did, off the UI thread; refresh after.
fn act(s: &Arc<Shared>, path: &'static str, body: Value, ok: Option<&'static str>) {
    let s = s.clone();
    std::thread::spawn(move || {
        let r = crate::server::post(&s, path, &body);
        let _ = slint::invoke_from_event_loop(move || {
            match r {
                Ok(_) => { if let Some(m) = ok { toast(m, false); } }
                Err(e) => toast(e, true),
            }
            refresh(true);
        });
    });
}

fn wire(win: &AppWindow, s: &Arc<Shared>) {
    let a = win.global::<AppState>();
    let w = win.as_weak();
    a.on_nav(move |page| {
        if let Some(win) = w.upgrade() {
            close_player(&win);
            win.global::<AppState>().set_page(page);
            refresh(true);
        }
    });
    let s1 = s.clone();
    a.on_mark(move || act(&s1, "/api/mark", json!({}), Some("Highlight saved")));
    let s1 = s.clone();
    a.on_stop(move || act(&s1, "/api/stop", json!({}), None));
    let s1 = s.clone();
    a.on_screen(move || act(&s1, "/api/screen", json!({}), None));
    let s1 = s.clone();
    a.on_answer(move |g, m| act(&s1, "/api/answer", json!({"name": g.as_str(), "mode": m.as_str()}), None));
    let s1 = s.clone();
    a.on_obs(move |what| match what.as_str() {
        "install" => act(&s1, "/api/obs/install", json!({}), Some("Installing OBS Studio in the background…")),
        "download" => act(&s1, "/api/obs/download", json!({}), None),
        _ => browse_obs(&s1),
    });

    // clips
    let c = win.global::<ClipsState>();
    c.on_filter(|| refresh(false));
    let s1 = s.clone();
    c.on_open_folder(move || act(&s1, "/api/open-folder", json!({}), None));
    let (s1, w) = (s.clone(), win.as_weak());
    c.on_open(move |id| { if let Some(win) = w.upgrade() { open_player(&win, &s1, &id); } });
    let s1 = s.clone();
    c.on_rename(move |id, t| {
        let t = t.trim().to_string();
        if t.is_empty() { return; }
        act(&s1, "/api/clip/update", json!({"id": id.as_str(), "title": t}), None);
    });
    let s1 = s.clone();
    c.on_delete(move |id, title| {
        if !crate::win::confirm("Fragtape", &format!("Move \"{title}\" to the Recycle Bin?")) { return; }
        act(&s1, "/api/clip/delete", json!({"id": id.as_str()}), Some("Moved to the Recycle Bin"));
    });
    let s1 = s.clone();
    c.on_favorite(move |id, fav| act(&s1, "/api/clip/update", json!({"id": id.as_str(), "favorite": fav}), None));
    let s1 = s.clone();
    c.on_reveal(move |id| act(&s1, "/api/clip/reveal", json!({"id": id.as_str()}), None));

    // games
    let g = win.global::<GamesState>();
    g.on_refilter(|| refresh(true));
    let s1 = s.clone();
    g.on_search(move |q| search(&s1, q.to_string()));
    let s1 = s.clone();
    g.on_add(move |id, exe| {
        let body = if id.is_empty() { json!({"exe": exe.as_str(), "name": exe.trim_end_matches(".exe").trim_end_matches(".EXE")}) } else { json!({"id": id.as_str()}) };
        with(|u| { u.win.global::<GamesState>().set_query(ss("")); u.win.global::<GamesState>().set_results(model(vec![])); });
        act(&s1, "/api/add", body, Some("Added"));
    });
    let s1 = s.clone();
    g.on_set_mode(move |name, mode| act(&s1, "/api/mode", json!({"name": name.as_str(), "mode": mode.as_str()}), None));
    let s1 = s.clone();
    g.on_set_default(move |mode| act(&s1, "/api/default", json!({"mode": mode.as_str()}), None));
    let s1 = s.clone();
    g.on_remove(move |name| act(&s1, "/api/remove", json!({"name": name.as_str()}), None));
    let s1 = s.clone();
    g.on_scan(move || act(&s1, "/api/scan", json!({}), None));

    // settings
    let st = win.global::<SettingsState>();
    let w = win.as_weak();
    st.on_pick_tab(move |t| {
        if let Some(win) = w.upgrade() {
            win.global::<SettingsState>().set_tab(t.clone());
            let _ = std::fs::write(crate::util::local_dir().join("window-tab.txt"), t.as_str());
            refresh(true);
        }
    });
    let (s1, w) = (s.clone(), win.as_weak());
    st.on_set(move |key, value| {
        let v = data::pref_value(&key, &value);
        // look settings apply straight away
        if let (Some(win), true) = (w.upgrade(), matches!(key.as_str(), "theme" | "accent" | "text_size" | "thumb_size" | "animations")) {
            let mut p = with(|u| u.state["prefs"].as_object().cloned()).flatten().unwrap_or_default();
            p.insert(key.to_string(), v.clone());
            theme::apply(&win, &p);
        }
        act(&s1, "/api/pref", json!({"key": key.as_str(), "value": v}), None);
    });
    let s1 = s.clone();
    st.on_action(move |key, value| match (key.as_str(), value.as_str()) {
        ("obs", "install") => act(&s1, "/api/obs/install", json!({}), Some("Installing OBS Studio in the background…")),
        ("obs", "browse") => browse_obs(&s1),
        ("obs", "auto") => act(&s1, "/api/obs/path", json!({"path": null}), Some("Finding OBS automatically")),
        ("test-card", _) => act(&s1, "/api/test-card", json!({}), None),
        ("folder", _) => act(&s1, "/api/open-folder", json!({}), None),
        ("ffmpeg", _) => act(&s1, "/api/install-ffmpeg", json!({}), Some("Installing ffmpeg in the background…")),
        ("advnote", "custom") => act(&s1, "/api/pref", json!({"key": "quality", "value": "custom"}), None),
        ("advreset", _) => {
            if crate::win::confirm("Fragtape", "Reset all advanced recording settings to their defaults?") {
                act(&s1, "/api/reset-advanced", json!({}), Some("Advanced settings reset"));
            }
        }
        ("gsi", _) => {
            let s2 = s1.clone();
            std::thread::spawn(move || {
                let r = crate::server::post(&s2, "/api/install-gsi", &json!({}));
                let _ = slint::invoke_from_event_loop(move || {
                    let note = match r {
                        Ok(v) => format!("{}. Restart the game if it's running.", v["results"].as_array().map(|x| x.iter().filter_map(|y| y.as_str()).collect::<Vec<_>>().join(" · ")).unwrap_or_default()),
                        Err(e) => e,
                    };
                    with(|u| u.gsi_note = note);
                    refresh(true);
                });
            });
        }
        _ => {}
    });

    wire_player(win, s);
}

fn browse_obs(s: &Arc<Shared>) {
    // the file picker is modal; it runs here so it belongs to this window
    match crate::app::browse_obs(s) {
        Ok(Some(p)) => toast(format!("Using OBS from {}", p.display()), false),
        Ok(None) => {}
        Err(e) => toast(e, true),
    }
    refresh(true);
}

fn search(s: &Arc<Shared>, q: String) {
    let s = s.clone();
    std::thread::spawn(move || {
        let results = if q.trim().len() >= 2 { crate::server::search_games(&s, q.trim()) } else { vec![] };
        let _ = slint::invoke_from_event_loop(move || {
            with(|u| {
                if u.win.global::<GamesState>().get_query() != q.as_str() { return; } // typed more since
                let ready = u.state["db"]["ready"] == true;
                let s = u.s.clone();
                let work = u.work.clone();
                let (rows, note) = data::found_rows(&q, &results, ready, &mut u.cache, |r| {
                    let name = r["name"].as_str().unwrap_or("");
                    let url = s.gamedb.get(r["id"].as_str().unwrap_or("")).and_then(|g| g.icon_url());
                    work.icon(name, Some(&json!({"name": name})), url)
                });
                u.win.global::<GamesState>().set_results(model(rows));
                u.win.global::<GamesState>().set_results_note(ss(note));
            });
        });
    });
}

// ---- player ------------------------------------------------------------------------------------------
fn volume_file() -> std::path::PathBuf {
    crate::util::local_dir().join("window-volume.txt")
}

fn open_player(win: &AppWindow, s: &Arc<Shared>, id: &str) {
    let Some(clip) = with(|u| u.clips.iter().find(|c| c["id"] == id).cloned()).flatten() else { return };
    if clip["live"] == true {
        return toast("Still recording: it opens once the game is closed", false);
    }
    let Some(path) = s.clips.path_for(id) else { return toast("The file is gone", true) };
    let (vol, muted) = std::fs::read_to_string(volume_file()).ok().and_then(|t| {
        let mut it = t.split_whitespace();
        Some((it.next()?.parse::<f64>().ok()?, it.next()? == "1"))
    }).unwrap_or((1.0, false));
    let Some(p) = player::Player::open(win, &clip, path, vol, muted) else { return toast("Can't open this video", true) };
    with(|u| {
        u.player = Some((p, clip));
        u.frames.start(TimerMode::Repeated, Duration::from_millis(15), || {
            with(|u| {
                let win = u.win.clone_strong();
                if let Some((p, c)) = u.player.as_mut() { p.tick(&win, c); }
            });
        });
    });
    win.global::<AppState>().set_player_open(true);
    let ff = with(|u| u.state["app"]["ffmpeg"] == true).unwrap_or(false);
    with(|u| if let Some((p, _)) = &u.player { p.draw_sel(&u.win, ff); });
}

fn close_player(win: &AppWindow) {
    with(|u| { u.player = None; u.frames.stop(); });
    if win.window().is_fullscreen() { win.window().set_fullscreen(false); }
    win.global::<PlayerState>().set_fullscreen(false);
    win.global::<AppState>().set_player_open(false);
}

fn with_player<R>(f: impl FnOnce(&mut player::Player, &AppWindow, &mut Value, bool) -> R) -> Option<R> {
    with(|u| {
        let win = u.win.clone_strong();
        let ff = u.state["app"]["ffmpeg"] == true;
        let (p, c) = u.player.as_mut()?;
        Some(f(p, &win, c, ff))
    }).flatten()
}

fn save_volume(v: f64, muted: bool) {
    let _ = std::fs::write(volume_file(), format!("{v} {}", if muted { 1 } else { 0 }));
}

fn wire_player(win: &AppWindow, s: &Arc<Shared>) {
    let p = win.global::<PlayerState>();
    p.on_toggle_play(|| { with_player(|p, _, _, _| p.toggle()); });
    p.on_seek(|f| { with_player(|p, _, _, _| p.press(f as f64)); });
    p.on_drag(|f| { with_player(|p, w, _, _| p.drag(w, f as f64)); });
    p.on_drag_end(|| { with_player(|p, _, _, _| p.release()); });
    let s1 = s.clone();
    p.on_hover(move |f| {
        with_player(|p, w, _, _| {
            if let Some(path) = s1.clips.path_for(&p.id) { p.hover(w, f as f64, &path); }
        });
    });
    p.on_hover_out(|| { with(|u| u.win.global::<PlayerState>().set_hover_on(false)); });
    p.on_jump(|t| { with_player(|p, _, _, _| p.seek_to(t as f64 - 3.0, true)); });
    p.on_jump_cat(|cat| { with_player(|p, _, _, _| { p.jump_cat(&cat); p.poke(); }); });
    p.on_step_moment(|dir| { with_player(|p, _, _, _| { p.jump_moment(dir); p.poke(); }); });
    p.on_poke(|| { with_player(|p, _, _, _| p.poke()); });
    p.on_set_volume(|v| {
        with_player(|p, w, _, _| {
            p.set_volume(v as f64);
            let st = w.global::<PlayerState>();
            st.set_volume(v);
            st.set_muted(v <= 0.0);
            save_volume(v as f64, v <= 0.0);
        });
    });
    p.on_toggle_mute(|| {
        with_player(|p, w, _, _| {
            let st = w.global::<PlayerState>();
            let m = !st.get_muted();
            let v = if !m && st.get_volume() <= 0.0 { 0.5 } else { st.get_volume() };
            st.set_muted(m);
            st.set_volume(v);
            p.set_volume(v as f64);
            p.set_muted(m);
            save_volume(v as f64, m);
        });
    });
    p.on_speed(|| { with_player(|p, w, _, _| { let t = p.speed(); w.global::<PlayerState>().set_rate_text(ss(t)); }); });
    p.on_toggle_fullscreen(|| {
        with(|u| {
            let w = u.win.window();
            let on = !w.is_fullscreen();
            w.set_fullscreen(on);
            u.win.global::<PlayerState>().set_fullscreen(on);
        });
    });
    p.on_trim_start(|| { with_player(|p, w, _, ff| { p.trim_start(); p.draw_sel(w, ff); }); });
    p.on_trim_cancel(|| { with_player(|p, w, _, ff| { p.stop_preview(); p.sel = None; p.draw_sel(w, ff); }); });
    p.on_trim_preview(|| { with_player(|p, _, _, _| { p.preview_sel(); p.poke(); }); });
    p.on_trim_move(|h, f| { with_player(|p, w, _, ff| { p.trim_move(h, f as f64); p.draw_sel(w, ff); }); });
    let s1 = s.clone();
    p.on_trim_save(move || {
        let Some((id, sel, title)) = with_player(|p, w, _, _| (p.id.clone(), p.sel, w.global::<PlayerState>().get_title().to_string())) else { return };
        let Some((a, b)) = sel else { return };
        with(|u| u.win.global::<PlayerState>().set_saving(true));
        let s2 = s1.clone();
        std::thread::spawn(move || {
            let r = crate::server::post(&s2, "/api/clip/trim", &json!({"id": id, "start": a, "end": b, "title": title}));
            let _ = slint::invoke_from_event_loop(move || {
                with(|u| u.win.global::<PlayerState>().set_saving(false));
                match r {
                    Ok(v) => {
                        toast(format!("Saved {}", v["name"].as_str().unwrap_or("the clip")), false);
                        with_player(|p, w, _, ff| { p.sel = None; p.draw_sel(w, ff); });
                    }
                    Err(e) => toast(e, true),
                }
                refresh(true);
            });
        });
    });
    let w = win.as_weak();
    p.on_back(move || { if let Some(win) = w.upgrade() { close_player(&win); refresh(true); } });
    let s1 = s.clone();
    p.on_fav(move || {
        let Some((id, fav)) = with_player(|p, w, c, _| {
            let f = c["favorite"] != true;
            c["favorite"] = json!(f);
            w.global::<PlayerState>().set_favorite(f);
            (p.id.clone(), f)
        }) else { return };
        act(&s1, "/api/clip/update", json!({"id": id, "favorite": fav}), None);
    });
    let s1 = s.clone();
    p.on_reveal(move || { if let Some(id) = with_player(|p, _, _, _| p.id.clone()) { act(&s1, "/api/clip/reveal", json!({"id": id}), None); } });
    let (s1, w) = (s.clone(), win.as_weak());
    p.on_delete(move || {
        let Some((id, title)) = with_player(|p, w, _, _| (p.id.clone(), w.global::<PlayerState>().get_title().to_string())) else { return };
        if !crate::win::confirm("Fragtape", &format!("Move \"{title}\" to the Recycle Bin?")) { return; }
        if let Some(win) = w.upgrade() { close_player(&win); }
        act(&s1, "/api/clip/delete", json!({"id": id}), Some("Moved to the Recycle Bin"));
    });
    let s1 = s.clone();
    p.on_rename(move |t| {
        let t = t.trim().to_string();
        if t.is_empty() { return; }
        if let Some(id) = with_player(|p, _, c, _| { c["title"] = json!(t); p.id.clone() }) {
            act(&s1, "/api/clip/update", json!({"id": id, "title": t}), None);
        }
    });
    let s1 = s.clone();
    p.on_install_ffmpeg(move || act(&s1, "/api/install-ffmpeg", json!({}), Some("Installing ffmpeg in the background…")));
    let w = win.as_weak();
    p.on_key(move |text, shift| {
        use slint::platform::Key;
        let k = text.as_str();
        let is = |key: Key| SharedString::from(key).as_str() == k;
        let Some(win) = w.upgrade() else { return };
        if is(Key::Escape) {
            if win.window().is_fullscreen() { win.global::<PlayerState>().invoke_toggle_fullscreen(); } else { close_player(&win); refresh(true); }
            return;
        }
        let st = win.global::<PlayerState>();
        with_player(|p, w, _, ff| {
            match k {
                " " | "k" | "K" => p.toggle(),
                "j" | "J" => p.nudge(-10.0),
                "l" | "L" => p.nudge(10.0),
                "," => p.step(-1.0),
                "." => p.step(1.0),
                "[" => { p.mark_in(); p.draw_sel(w, ff); }
                "]" => { p.mark_out(); p.draw_sel(w, ff); }
                _ if is(Key::LeftArrow) => if shift { p.jump_moment(-1) } else { p.nudge(-5.0) },
                _ if is(Key::RightArrow) => if shift { p.jump_moment(1) } else { p.nudge(5.0) },
                _ => {}
            }
            p.poke();
        });
        match k {
            "m" | "M" => st.invoke_toggle_mute(),
            "f" | "F" => st.invoke_toggle_fullscreen(),
            _ => {}
        }
    });
}
