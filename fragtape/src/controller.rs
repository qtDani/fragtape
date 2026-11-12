//! Core loop: detect games, start/stop recording per game mode, collect highlight markers.
//!
//! Modes: full (record the whole session; events become chapters), highlights
//! (replay buffer only; a clip is saved when a moment gets hot), off.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::engine::{Obs, Target};
use crate::events::{find_highlights, known_games, title_for, EventBus, Game, GameEvent, Highlight, Marker, Session};
use crate::gamedb::GameDb;
use crate::library::{Library, ASK, FULL, HIGHLIGHTS, OFF};
use crate::providers::LeagueProvider;
use crate::util::{mono, unix_now};
use crate::win::{self, Snapshot};
use crate::logf;

#[derive(Clone)]
pub struct Settings {
    pub stop_on_exit: bool,
    pub exit_grace: f64,
    pub add_chapters: bool,
    pub replay_delay: f64,
    pub replay_max_wait: f64,
    pub min_score: f64,
    pub merge_gap: f64,
    pub pre_roll: f64,
    pub post_roll: f64,
    pub poll_interval: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { stop_on_exit: true, exit_grace: 10.0, add_chapters: true, replay_delay: 5.0, replay_max_wait: 15.0,
                   min_score: 2.0, merge_gap: 10.0, pre_roll: 8.0, post_roll: 4.0, poll_interval: 2.0 }
    }
}

/// Finished videos for the app to process (wait for the file, add to the library, cut clips).
pub enum Media {
    Session { path: String, game: Option<String>, markers: Vec<Marker>, highlights: Vec<Highlight> },
    Clip { path: String, game: Option<String>, title: String },
}

pub type Answers = Arc<Mutex<VecDeque<(String, Option<String>)>>>;
/// Shows the first-time question (game name, its processes); returns a function that closes it.
pub type Asker = Box<dyn Fn(&str, &[u32], Answers) -> Option<Box<dyn FnOnce() + Send>> + Send>;

struct Moment {
    first: f64,
    last: f64,
    score: f64,
    events: Vec<GameEvent>,
}

pub struct Controller<O: Obs> {
    pub obs: O,
    pub settings: Settings,
    pub library: Arc<Library>,
    pub gamedb: Arc<GameDb>,
    pub bus: EventBus,
    pub answers: Answers,
    pub outbox: Vec<Media>,
    processes: Box<dyn Fn() -> Snapshot + Send>,
    asker: Asker,
    pub current: Option<Game>,
    current_info: Value,
    current_since: f64,
    pub current_exe: Option<String>,
    pub current_pids: Vec<u32>,
    pub mode: Option<String>,
    game_gone_since: Option<f64>,
    we_started_recording: bool,
    we_started_replay: bool,
    session: Option<Session>,
    next_poll: f64,
    moment: Option<Moment>,
    replay_due: Option<f64>,
    replay_titles: VecDeque<(String, Option<String>)>,
    asking: BTreeSet<String>,
    close_prompt: HashMap<String, Box<dyn FnOnce() + Send>>,
    lol: Option<LeagueProvider>,
    pub start_providers: bool,
    window_found: bool,
    /// A screen recording the user started is running (or starting).
    pub screen_rec: bool,
    screen_since: f64,
    /// The game wanted a full recording while the screen was being recorded: start it after.
    game_rec_waiting: bool,
}

pub fn exe_running(exe: &str, running: &Snapshot) -> bool {
    let base = exe.rsplit('/').next().unwrap_or(exe);
    if !running.contains(base) {
        return false;
    }
    !exe.contains('/') || running.paths(base).iter().any(|p| p.ends_with(&format!("/{exe}")))
}

impl<O: Obs> Controller<O> {
    pub fn new(obs: O, library: Arc<Library>, gamedb: Arc<GameDb>, processes: Box<dyn Fn() -> Snapshot + Send>, asker: Asker) -> Self {
        Controller {
            obs, settings: Settings::default(), library, gamedb, bus: EventBus::default(), answers: Answers::default(),
            outbox: vec![], processes, asker, current: None, current_info: json!({}), current_since: 0.0, current_exe: None,
            current_pids: vec![], mode: None, game_gone_since: None, we_started_recording: false, we_started_replay: false,
            session: None, next_poll: 0.0, moment: None, replay_due: None, replay_titles: VecDeque::new(),
            asking: BTreeSet::new(), close_prompt: HashMap::new(), lol: None, start_providers: true, window_found: false,
            screen_rec: false, screen_since: 0.0, game_rec_waiting: false,
        }
    }

    // ---- detection ---------------------------------------------------------------------
    pub fn detect(&self, running: &Snapshot) -> Option<(Game, Value)> {
        for g in known_games() {
            if g.exes.iter().any(|e| running.contains(e)) {
                let mut info = json!({"source": "builtin", "exes": g.exes});
                if let Some(db) = self.gamedb.for_exe(&g.exes[0]) {
                    info["discord_id"] = json!(db.id);
                    info["icon"] = json!(db.icon);
                }
                return Some((g, info));
            }
        }
        for (exe, name) in self.library.custom_exes() {
            if exe_running(&exe, running) {
                return Some((Game { name, exes: vec![exe], provider: None }, json!({})));
            }
        }
        self.gamedb.find_running(running).map(|db| {
            (Game { name: db.name.clone(), exes: db.exes.clone(), provider: None },
             json!({"source": "discord", "discord_id": db.id, "icon": db.icon, "exes": db.exes}))
        })
    }

    pub fn status(&self) -> Value {
        json!({
            "game": self.current.as_ref().map(|g| g.name.clone()),
            "mode": self.mode,
            "since": self.current_since,
            "asking": self.asking.iter().collect::<Vec<_>>(),
            "recording": self.obs.recording(),
            "replay": self.obs.replay(),
            "window_found": self.window_found,
            "screen": self.screen_rec,
            "screen_since": self.screen_since,
        })
    }

    // ---- main loop ------------------------------------------------------------------------
    pub fn tick(&mut self) {
        let now = mono();
        let pending: Vec<_> = self.answers.lock().unwrap().drain(..).collect();
        for (game, mode) in pending {
            self.on_answer(&game, mode);
        }
        if now >= self.next_poll {
            self.next_poll = now + self.settings.poll_interval;
            self.poll(now);
        }
        for e in self.bus.drain() {
            self.handle_event(e);
        }
        if self.replay_due.is_some_and(|t| now >= t) {
            self.flush_moment();
        }
    }

    fn poll(&mut self, now: f64) {
        let running = (self.processes)();
        let found = self.detect(&running);
        match (found, &self.current) {
            (Some((game, info)), cur) if cur.as_ref().map_or(true, |c| c.name != game.name) => {
                if self.current.is_some() {
                    self.game_ended();
                }
                self.game_started(game, info, &running);
            }
            (Some(_), _) => {
                self.game_gone_since = None;
                self.refresh_window();
            }
            (None, Some(_)) => match self.game_gone_since {
                None => self.game_gone_since = Some(now),
                Some(t) if now - t >= self.settings.exit_grace => self.game_ended(),
                _ => {}
            },
            (None, None) => {}
        }
    }

    fn game_started(&mut self, game: Game, info: Value, running: &Snapshot) {
        self.current_since = unix_now();
        self.mode = None;
        self.game_gone_since = None;
        self.current_exe = game.exes.iter().find(|e| exe_running(e, running)).map(|e| e.rsplit('/').next().unwrap().to_string());
        self.current_pids = self.current_exe.as_ref().map(|e| running.pids_of(e)).unwrap_or_default();
        let mut info = info;
        if let Some(path) = self.current_exe.as_ref().and_then(|e| running.paths(e).into_iter().next()) {
            info["exe_path"] = json!(path); // for the game's icon
        }
        self.library.game_started(&game.name, info.clone());
        self.current_info = info;
        let mode = self.library.mode_for(&game.name);
        logf!("game detected: {} (mode: {mode})", game.name);
        let name = game.name.clone();
        self.current = Some(game);
        if mode == ASK {
            if self.asking.insert(name.clone()) {
                logf!("asking what to record for {name}");
                if let Some(close) = (self.asker)(&name, &self.current_pids, self.answers.clone()) {
                    self.close_prompt.insert(name, close);
                }
            }
            return;
        }
        self.activate(&mode);
    }

    pub fn answer(&self, game: &str, mode: Option<String>) {
        self.answers.lock().unwrap().push_back((game.to_string(), mode));
    }

    fn on_answer(&mut self, game: &str, mode: Option<String>) {
        self.asking.remove(game);
        if let Some(close) = self.close_prompt.remove(game) {
            close(); // answered in the app window: close the popup
        }
        let mode = match mode.filter(|m| [FULL, HIGHLIGHTS, OFF].contains(&m.as_str())) {
            Some(m) => {
                let _ = self.library.set_mode(game, &m);
                logf!("saved mode for {game}: {m}");
                m
            }
            None => {
                logf!("no choice made for {game}: not recording this time, will ask again next time");
                OFF.to_string()
            }
        };
        if self.current.as_ref().is_some_and(|c| c.name == game) && self.mode.is_none() {
            self.activate(&mode);
        }
    }

    fn target(&mut self) -> Target {
        let spec = self.current_exe.clone().map(|exe| {
            let w = win::main_window(&self.current_pids);
            self.window_found = w.is_some();
            win::obs_window_spec(&exe, w.as_ref())
        });
        Target { spec, game: self.current.as_ref().map(|g| g.name.clone()).unwrap_or_default(), pids: self.current_pids.clone(), screen: false }
    }

    fn activate(&mut self, mode: &str) {
        let Some(game) = self.current.clone() else { return };
        self.mode = Some(mode.to_string());
        if mode == OFF {
            return logf!("not recording {}", game.name);
        }
        if game.provider == Some("lol") && self.start_providers && self.lol.is_none() {
            self.lol = Some(LeagueProvider::start(self.bus.clone()));
        }
        if mode == FULL && self.screen_rec {
            logf!("{} will be recorded when the screen recording stops", game.name);
            self.game_rec_waiting = true;
        } else if mode == FULL && !self.obs.recording() {
            logf!("starting recording");
            let t = self.target();
            self.obs.start_recording(t);
            self.we_started_recording = true;
        } else if mode == HIGHLIGHTS && !self.obs.replay() {
            logf!("starting replay buffer (highlights only)");
            let t = self.target();
            self.obs.start_replay(t);
            self.we_started_replay = true;
        }
    }

    fn refresh_window(&mut self) {
        // Games often open their real window a few seconds after the process starts.
        if !self.window_found && (self.obs.recording() || self.obs.replay()) {
            if let Some(exe) = self.current_exe.clone() {
                if let Some(w) = win::main_window(&self.current_pids) {
                    self.window_found = true;
                    self.obs.update_window(&win::obs_window_spec(&exe, Some(&w)));
                }
            }
        }
    }

    fn game_ended(&mut self) {
        if self.replay_due.is_some() {
            self.flush_moment(); // don't lose a clip waiting for its aftermath
        }
        let game = self.current.take();
        if let Some(g) = &game {
            self.library.game_ended(&g.name, unix_now() - self.current_since);
            // Not answered while it was running: drop the question, ask again next time.
            if self.asking.remove(&g.name) {
                if let Some(close) = self.close_prompt.remove(&g.name) {
                    close();
                }
                logf!("no choice made for {}: will ask again next time", g.name);
            }
        }
        self.mode = None;
        self.game_gone_since = None;
        self.lol = None;
        logf!("game closed: {}", game.as_ref().map(|g| g.name.as_str()).unwrap_or("?"));
        if self.we_started_recording && self.settings.stop_on_exit && self.obs.recording() {
            logf!("stopping recording");
            self.obs.stop_recording();
        }
        if self.we_started_replay && self.obs.replay() {
            logf!("stopping replay buffer");
            self.obs.stop_replay();
        }
        self.we_started_recording = false;
        self.we_started_replay = false;
        self.game_rec_waiting = false;
        self.current_exe = None;
        self.current_pids.clear();
        self.window_found = false;
    }

    // ---- events ---------------------------------------------------------------------------
    fn handle_event(&mut self, e: GameEvent) {
        let manual = e.kind == "manual";
        if !manual && self.current.as_ref().map_or(true, |c| c.name != e.game) {
            return;
        }
        let full = self.mode.as_deref() == Some(FULL);
        if let Some(session) = self.session.as_mut().filter(|_| full || manual) {
            let m = session.add(&e);
            logf!("highlight event @ {:.1}s: {} {}", m.offset, m.label, m.detail);
            if self.settings.add_chapters {
                self.obs.add_chapter(&m.label);
            }
        } else if self.mode.as_deref() == Some(HIGHLIGHTS) || manual {
            self.add_to_moment(e);
        }
    }

    fn add_to_moment(&mut self, e: GameEvent) {
        let now = mono();
        let s = self.settings.clone();
        if self.moment.as_ref().map_or(true, |m| now - m.last > s.merge_gap) {
            self.moment = Some(Moment { first: now, last: now, score: 0.0, events: vec![] });
        }
        let m = self.moment.as_mut().unwrap();
        m.last = now;
        m.score += e.weight;
        logf!("[highlights] {} {} (moment score {})", e.label(), e.detail, m.score);
        m.events.push(e);
        if m.score >= s.min_score {
            // Wait for the aftermath, but not so long that the start falls out of the buffer.
            self.replay_due = Some((now + s.replay_delay).min(m.first + s.replay_max_wait));
        }
    }

    fn flush_moment(&mut self) {
        let events = self.moment.take().map(|m| m.events).unwrap_or_default();
        self.replay_due = None;
        let mut tmp = Session::new(0.0, None);
        let markers: Vec<Marker> = events.iter().map(|e| tmp.add(e)).collect();
        let title = title_for(&markers);
        if !self.obs.replay() {
            return logf!("[replay] '{title}' not saved: the replay buffer isn't running");
        }
        self.replay_titles.push_back((title.clone(), self.current.as_ref().map(|g| g.name.clone())));
        self.obs.save_replay();
        logf!("[replay] saving clip: {title}");
    }

    /// Start or stop a screen recording. Returns what happened, for a notification.
    pub fn toggle_screen(&mut self) -> &'static str {
        if self.screen_rec {
            logf!("stopping the screen recording");
            self.obs.stop_recording();
            return "stopped";
        }
        if self.obs.recording() {
            logf!("screen recording not started: the game is already being recorded");
            return "busy";
        }
        logf!("starting a screen recording");
        self.screen_rec = true;
        self.screen_since = unix_now();
        self.obs.start_recording(Target { screen: true, ..Target::default() });
        if !self.obs.recording() {
            self.screen_rec = false; // OBS missing or failed to start (logged)
            return "failed";
        }
        "started"
    }

    pub fn mark_manual(&self) {
        let name = self.current.as_ref().map(|g| g.name.clone()).unwrap_or_else(|| "Manual".into());
        self.bus.emit(GameEvent::new("manual", &name, "hotkey"));
    }

    // ---- engine events ------------------------------------------------------------------
    pub fn on_recording_started(&mut self) {
        let game = if self.screen_rec { Some(crate::app::SCREEN_GAME.to_string()) } else { self.current.as_ref().map(|g| g.name.clone()) };
        self.session = Some(Session::new(mono(), game));
        logf!("recording started, session open");
    }

    pub fn on_recording_stopped(&mut self, path: Option<String>) {
        if std::mem::take(&mut self.screen_rec) && std::mem::take(&mut self.game_rec_waiting) && self.mode.as_deref() == Some(FULL) {
            if let Some(mode) = self.mode.clone() {
                self.activate(&mode); // the game started during the screen recording
            }
        }
        let (Some(session), Some(path)) = (self.session.take(), path) else { return };
        let s = &self.settings;
        let highlights = find_highlights(&session.markers, s.merge_gap, s.min_score, s.pre_roll, s.post_roll);
        logf!("session done: {} events, {} highlights", session.markers.len(), highlights.len());
        self.outbox.push(Media::Session { path, game: session.game, markers: session.markers, highlights });
    }

    pub fn on_replay_saved(&mut self, path: Option<String>) {
        let (Some(path), Some((title, game))) = (path, self.replay_titles.pop_front()) else { return };
        self.outbox.push(Media::Clip { path, game, title });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::tempdir;

    #[derive(Default)]
    pub struct FakeObs {
        pub rec: bool,
        pub rb: bool,
        pub calls: Vec<String>,
    }

    impl Obs for FakeObs {
        fn recording(&self) -> bool { self.rec }
        fn replay(&self) -> bool { self.rb }
        fn start_recording(&mut self, t: Target) { self.rec = true; self.calls.push(format!("start_recording {}", t.spec.unwrap_or_default())); }
        fn stop_recording(&mut self) { self.rec = false; self.calls.push("stop_recording".into()); }
        fn add_chapter(&mut self, name: &str) -> bool { self.calls.push(format!("chapter {name}")); true }
        fn start_replay(&mut self, _: Target) { self.rb = true; self.calls.push("start_replay".into()); }
        fn stop_replay(&mut self) { self.rb = false; self.calls.push("stop_replay".into()); }
        fn save_replay(&mut self) { self.calls.push("save_replay".into()); }
        fn update_window(&mut self, _: &str) {}
    }

    fn ctl(running: Arc<Mutex<Vec<&'static str>>>, default_mode: &str) -> (tempdir::Dir, Controller<FakeObs>) {
        let d = tempdir::Dir::new();
        let lib = Arc::new(Library::open(d.0.join("library.json")));
        lib.set_default_mode(default_mode).unwrap();
        let procs = Box::new(move || Snapshot::from_names(&running.lock().unwrap()));
        let asker: Asker = Box::new(|_, _, _| None);
        let mut c = Controller::new(FakeObs::default(), lib, GameDb::new(d.0.clone()), procs, asker);
        c.start_providers = false;
        c.settings.poll_interval = 0.0;
        c.settings.exit_grace = 0.0;
        (d, c)
    }

    #[test]
    fn records_whole_session_with_chapters() {
        let running = Arc::new(Mutex::new(vec!["cs2.exe"]));
        let (_d, mut c) = ctl(running.clone(), FULL);
        c.tick();
        assert_eq!(c.status()["game"], "Counter-Strike 2");
        assert!(c.obs.calls[0].starts_with("start_recording ::cs2.exe"));
        c.on_recording_started();
        c.bus.emit(GameEvent::new("kill", "Counter-Strike 2", ""));
        c.bus.emit(GameEvent::new("headshot", "Other Game", "")); // not the current game
        c.tick();
        assert!(c.obs.calls.contains(&"chapter Kill".to_string()));
        assert_eq!(c.obs.calls.iter().filter(|x| x.starts_with("chapter")).count(), 1);
        running.lock().unwrap().clear();
        c.tick(); // gone: grace starts
        c.tick(); // grace over
        assert!(c.obs.calls.contains(&"stop_recording".to_string()));
        c.on_recording_stopped(Some("a.mp4".into()));
        assert!(matches!(&c.outbox[0], Media::Session { markers, .. } if markers.len() == 1));
        assert!(c.library.games().iter().any(|g| g["name"] == "Counter-Strike 2" && g["sessions"] == 1));
    }

    #[test]
    fn asks_once_and_applies_answer() {
        let running = Arc::new(Mutex::new(vec!["r5apex.exe"]));
        let (_d, mut c) = ctl(running, ASK);
        c.tick();
        assert_eq!(c.status()["asking"], json!(["Apex Legends"]));
        assert!(c.obs.calls.is_empty());
        c.answer("Apex Legends", Some(HIGHLIGHTS.into()));
        c.tick();
        assert_eq!(c.obs.calls, vec!["start_replay"]);
        assert_eq!(c.library.mode_for("Apex Legends"), HIGHLIGHTS);
    }

    #[test]
    fn highlights_mode_saves_a_clip_for_a_hot_moment() {
        let running = Arc::new(Mutex::new(vec!["r5apex.exe"]));
        let (_d, mut c) = ctl(running, HIGHLIGHTS);
        c.settings.replay_delay = 0.0;
        c.tick();
        c.bus.emit(GameEvent::new("kill", "Apex Legends", ""));
        c.tick();
        assert!(!c.obs.calls.contains(&"save_replay".to_string())); // score 1 < 2
        c.bus.emit(GameEvent::new("kill", "Apex Legends", ""));
        c.tick();
        c.tick();
        assert!(c.obs.calls.contains(&"save_replay".to_string()));
        c.on_replay_saved(Some("Clip.mp4".into()));
        assert!(matches!(&c.outbox[0], Media::Clip { title, .. } if title == "2 Kills"));
    }

    #[test]
    fn unanswered_question_is_asked_again_next_launch() {
        let running = Arc::new(Mutex::new(vec!["r5apex.exe"]));
        let (_d, mut c) = ctl(running.clone(), ASK);
        c.tick();
        running.lock().unwrap().clear();
        c.tick();
        c.tick();
        assert_eq!(c.status()["asking"], json!([]));
        running.lock().unwrap().push("r5apex.exe");
        c.tick();
        assert_eq!(c.status()["asking"], json!(["Apex Legends"]));
    }

    #[test]
    fn off_and_no_answer_do_nothing() {
        let running = Arc::new(Mutex::new(vec!["r5apex.exe"]));
        let (_d, mut c) = ctl(running, ASK);
        c.tick();
        c.answer("Apex Legends", None);
        c.tick();
        assert!(c.obs.calls.is_empty());
        assert_eq!(c.library.mode_for("Apex Legends"), ASK); // asked again next time
    }

    #[test]
    fn screen_recording_toggles_and_lands_in_its_own_group() {
        let running = Arc::new(Mutex::new(vec![]));
        let (_d, mut c) = ctl(running, FULL);
        assert_eq!(c.toggle_screen(), "started");
        assert_eq!(c.obs.calls, vec!["start_recording "]);
        assert_eq!(c.status()["screen"], true);
        c.on_recording_started();
        c.mark_manual();
        c.tick();
        assert!(c.obs.calls.contains(&"chapter Highlight".to_string()) || c.obs.calls.iter().any(|x| x.starts_with("chapter")));
        assert_eq!(c.toggle_screen(), "stopped");
        assert!(c.obs.calls.contains(&"stop_recording".to_string()));
        c.on_recording_stopped(Some("s.mp4".into()));
        assert!(!c.screen_rec);
        assert!(matches!(&c.outbox[0], Media::Session { game: Some(g), markers, .. } if g == crate::app::SCREEN_GAME && markers.len() == 1));
    }

    #[test]
    fn game_waits_for_the_screen_recording_then_records() {
        let running = Arc::new(Mutex::new(vec![]));
        let (_d, mut c) = ctl(running.clone(), FULL);
        c.toggle_screen();
        c.on_recording_started();
        running.lock().unwrap().push("cs2.exe");
        c.tick();
        assert_eq!(c.obs.calls.iter().filter(|x| x.starts_with("start_recording")).count(), 1); // still the screen
        c.toggle_screen();
        c.on_recording_stopped(Some("s.mp4".into()));
        assert!(c.obs.calls.last().unwrap().starts_with("start_recording ::cs2.exe"));
        assert_eq!(c.toggle_screen(), "busy"); // the game is being recorded now
    }

    #[test]
    fn folder_qualified_exes() {
        let s = Snapshot::with_paths(&[("game.exe", "d:/games/elden ring/game/game.exe")]);
        assert!(exe_running("elden ring/game/game.exe", &s));
        assert!(!exe_running("other/game.exe", &s));
        assert!(exe_running("game.exe", &s));
    }
}
