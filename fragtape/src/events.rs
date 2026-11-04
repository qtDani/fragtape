//! Game events, built-in games, recording sessions and highlight detection.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::util::mono;

// ---- events ------------------------------------------------------------------------------
pub fn weight(kind: &str) -> f64 {
    match kind {
        "kill" => 1.0, "headshot" => 1.2, "assist" => 0.3, "death" => 0.0,
        "double_kill" => 1.0, "triple_kill" => 2.0, "quad_kill" => 3.0, "penta_kill" => 5.0,
        "ace" => 4.0, "first_blood" => 1.0, "objective" => 1.5, "objective_steal" => 4.0,
        "structure" => 0.5, "mvp" => 1.0, "manual" => 10.0,
        _ => 1.0,
    }
}

pub fn label(kind: &str) -> String {
    match kind {
        "kill" => "Kill", "headshot" => "Headshot", "assist" => "Assist", "death" => "Death",
        "double_kill" => "Double Kill", "triple_kill" => "Triple Kill", "quad_kill" => "Quadra Kill",
        "penta_kill" => "Penta Kill", "ace" => "Ace", "first_blood" => "First Blood", "objective" => "Objective",
        "objective_steal" => "Objective Steal", "structure" => "Structure", "mvp" => "MVP",
        "manual" => "Manual Highlight",
        other => return other.replace('_', " "),
    }
    .to_string()
}

pub fn is_kill(kind: &str) -> bool {
    kind == "kill" || kind == "headshot"
}

#[derive(Clone, Debug)]
pub struct GameEvent {
    pub kind: String,
    pub game: String,
    pub detail: String,
    pub t: f64,
    pub weight: f64,
}

impl GameEvent {
    pub fn new(kind: &str, game: &str, detail: impl Into<String>) -> Self {
        GameEvent { kind: kind.into(), game: game.into(), detail: detail.into(), t: mono(), weight: weight(kind) }
    }
    pub fn label(&self) -> String {
        label(&self.kind)
    }
}

/// Thread-safe hand-off from detection threads to the main loop.
#[derive(Clone, Default)]
pub struct EventBus(Arc<Mutex<VecDeque<GameEvent>>>);

impl EventBus {
    pub fn emit(&self, e: GameEvent) {
        self.0.lock().unwrap().push_back(e);
    }
    pub fn drain(&self) -> Vec<GameEvent> {
        self.0.lock().unwrap().drain(..).collect()
    }
}

// ---- games ---------------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq)]
pub struct Game {
    pub name: String,
    pub exes: Vec<String>,
    pub provider: Option<&'static str>, // "lol", "cs2", "dota2"
}

pub const KNOWN_GAMES: &[(&str, &[&str], Option<&str>)] = &[
    ("League of Legends", &["league of legends.exe"], Some("lol")),
    ("Counter-Strike 2", &["cs2.exe"], Some("cs2")),
    ("Dota 2", &["dota2.exe"], Some("dota2")),
    ("VALORANT", &["valorant-win64-shipping.exe"], None),
    ("Fortnite", &["fortniteclient-win64-shipping.exe"], None),
    ("Apex Legends", &["r5apex.exe", "r5apex_dx12.exe"], None),
    ("Overwatch 2", &["overwatch.exe"], None),
    ("Rocket League", &["rocketleague.exe"], None),
    ("Call of Duty", &["cod.exe", "cod24-cod.exe", "blackops6.exe"], None),
    ("Rainbow Six Siege", &["rainbowsix.exe", "rainbowsix_vulkan.exe", "rainbowsix_be.exe"], None),
    ("PUBG: Battlegrounds", &["tslgame.exe"], None),
    ("Marvel Rivals", &["marvel-win64-shipping.exe"], None),
    ("Deadlock", &["deadlock.exe", "project8.exe"], None),
    ("Escape from Tarkov", &["escapefromtarkov.exe"], None),
    ("Minecraft", &["minecraft.windows.exe"], None),
];

pub fn known_games() -> Vec<Game> {
    KNOWN_GAMES
        .iter()
        .map(|(n, e, p)| Game { name: n.to_string(), exes: e.iter().map(|s| s.to_string()).collect(), provider: *p })
        .collect()
}

pub fn has_kill_detection(name: &str) -> bool {
    KNOWN_GAMES.iter().any(|(n, _, p)| p.is_some() && n.eq_ignore_ascii_case(name))
}

// ---- sessions + highlights ----------------------------------------------------------------------
#[derive(Clone, Debug, Serialize)]
pub struct Marker {
    pub offset: f64,
    pub kind: String,
    pub label: String,
    pub game: String,
    pub detail: String,
    pub weight: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Highlight {
    pub start: f64,
    pub end: f64,
    pub score: f64,
    pub title: String,
    pub markers: Vec<Marker>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub started_mono: f64,
    pub game: Option<String>,
    pub markers: Vec<Marker>,
}

impl Session {
    pub fn new(started_mono: f64, game: Option<String>) -> Self {
        Session { started_mono, game, markers: vec![] }
    }

    pub fn add(&mut self, e: &GameEvent) -> Marker {
        let m = Marker {
            offset: ((e.t - self.started_mono).max(0.0) * 100.0).round() / 100.0,
            kind: e.kind.clone(),
            label: e.label(),
            game: e.game.clone(),
            detail: e.detail.clone(),
            weight: e.weight,
        };
        self.markers.push(m.clone());
        if self.game.is_none() {
            self.game = Some(e.game.clone());
        }
        m
    }
}

pub fn title_for(markers: &[Marker]) -> String {
    let Some(best) = markers.iter().max_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap()) else {
        return "Highlight".into();
    };
    let kills = markers.iter().filter(|m| is_kill(&m.kind)).count();
    let title = label(&best.kind);
    if is_kill(&best.kind) {
        return if kills >= 2 { format!("{kills} Kills") } else { title };
    }
    if kills >= 2 { format!("{title} ({kills} kills)") } else { title }
}

/// Group nearby markers into moments and keep the hot ones (see the README).
pub fn find_highlights(markers: &[Marker], merge_gap: f64, min_score: f64, pre: f64, post: f64) -> Vec<Highlight> {
    let mut ordered = markers.to_vec();
    ordered.sort_by(|a, b| a.offset.partial_cmp(&b.offset).unwrap());
    let mut clusters: Vec<Vec<Marker>> = vec![];
    for m in ordered {
        match clusters.last_mut() {
            Some(c) if m.offset - c.last().unwrap().offset <= merge_gap => c.push(m),
            _ => clusters.push(vec![m]),
        }
    }
    let mut out: Vec<Highlight> = vec![];
    for c in clusters {
        let score: f64 = c.iter().map(|m| m.weight).sum();
        if score < min_score {
            continue;
        }
        let start = (c[0].offset - pre).max(0.0);
        let end = c.last().unwrap().offset + post;
        match out.last_mut() {
            Some(prev) if start <= prev.end => {
                prev.markers.extend(c);
                prev.end = prev.end.max(end);
                prev.score += score;
                prev.title = title_for(&prev.markers);
            }
            _ => out.push(Highlight { start, end, score, title: title_for(&c), markers: c }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(offset: f64, kind: &str) -> Marker {
        Marker { offset, kind: kind.into(), label: label(kind), game: "G".into(), detail: "".into(), weight: weight(kind) }
    }

    #[test]
    fn highlights_group_and_score() {
        let ms = vec![m(10.0, "kill"), m(14.0, "kill"), m(100.0, "kill"), m(200.0, "triple_kill")];
        let h = find_highlights(&ms, 10.0, 2.0, 8.0, 4.0);
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].start, h[0].end, h[0].title.as_str()), (2.0, 18.0, "2 Kills"));
        assert_eq!(h[1].title, "Triple Kill");
    }

    #[test]
    fn overlapping_highlights_merge() {
        // separate moments (gap 11 s > 10 s) whose clips overlap (13 s < 14 s): one clip
        let ms = vec![m(10.0, "manual"), m(21.0, "manual")];
        let h = find_highlights(&ms, 10.0, 2.0, 8.0, 4.0);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].start, h[0].end, h[0].markers.len()), (2.0, 25.0, 2));
    }

    #[test]
    fn session_offsets() {
        let mut s = Session::new(100.0, None);
        let mut e = GameEvent::new("kill", "CS", "x");
        e.t = 112.345;
        assert_eq!(s.add(&e).offset, 12.35);
        assert_eq!(s.game.as_deref(), Some("CS"));
    }
}
