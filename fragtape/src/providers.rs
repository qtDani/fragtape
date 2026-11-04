//! Kill detection: League of Legends (Live Client Data API) and CS2 / Dota 2 (Game State Integration).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::events::{EventBus, GameEvent};
use crate::{http, logf};

// ---- League of Legends ---------------------------------------------------------------------
const LOL: &str = "League of Legends";

fn base(name: &Value) -> String {
    name.as_str().unwrap_or("").split('#').next().unwrap_or("").trim().to_lowercase()
}

fn is_me(name: &Value, me: &str) -> bool {
    let n = base(name);
    !n.is_empty() && n == base(&Value::String(me.into()))
}

fn involved(e: &Value, me: &str) -> bool {
    is_me(&e["KillerName"], me) || e["Assisters"].as_array().is_some_and(|a| a.iter().any(|x| is_me(x, me)))
}

/// One Live Client Data event -> GameEvents for player `me`.
pub fn lol_translate(e: &Value, me: &str) -> Vec<GameEvent> {
    let s = |k: &str| e[k].as_str().unwrap_or("?").to_string();
    let ev = |kind: &str, detail: String| vec![GameEvent::new(kind, LOL, detail)];
    match e["EventName"].as_str().unwrap_or("") {
        "ChampionKill" if is_me(&e["KillerName"], me) => ev("kill", format!("killed {}", s("VictimName"))),
        "ChampionKill" if is_me(&e["VictimName"], me) => ev("death", format!("killed by {}", s("KillerName"))),
        "ChampionKill" if e["Assisters"].as_array().is_some_and(|a| a.iter().any(|x| is_me(x, me))) =>
            ev("assist", format!("assisted on {}", s("VictimName"))),
        "Multikill" if is_me(&e["KillerName"], me) => match e["KillStreak"].as_i64().unwrap_or(0) {
            2 => ev("double_kill", "".into()), 3 => ev("triple_kill", "".into()),
            4 => ev("quad_kill", "".into()), 5 => ev("penta_kill", "".into()), _ => vec![],
        },
        "Ace" if is_me(&e["Acer"], me) => ev("ace", "".into()),
        "FirstBlood" if is_me(&e["Recipient"], me) => ev("first_blood", "".into()),
        name @ ("DragonKill" | "BaronKill" | "HeraldKill" | "HordeKill" | "AtakhanKill") if involved(e, me) => {
            let what = match e["DragonType"].as_str() {
                Some(t) => format!("{t} Dragon"),
                None => match name { "DragonKill" => "Dragon", "BaronKill" => "Baron Nashor", "HeraldKill" => "Rift Herald",
                                     "HordeKill" => "Voidgrub", _ => "Atakhan" }.into(),
            };
            let stolen = e["Stolen"].as_str().is_some_and(|x| x.eq_ignore_ascii_case("true")) || e["Stolen"] == true;
            ev(if stolen { "objective_steal" } else { "objective" }, what)
        }
        "TurretKilled" if is_me(&e["KillerName"], me) => ev("structure", "Turret".into()),
        "InhibKilled" if is_me(&e["KillerName"], me) => ev("structure", "Inhibitor".into()),
        _ => vec![],
    }
}

pub struct LeagueProvider {
    stop: Arc<AtomicBool>,
}

impl LeagueProvider {
    pub fn start(bus: EventBus) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::Builder::new().name("lol".into()).spawn(move || {
            let base = "https://127.0.0.1:2999/liveclientdata";
            let mut me: Option<String> = None;
            let mut seen: Option<HashSet<i64>> = None;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                if me.is_none() {
                    match http::get_json(&format!("{base}/activeplayername"), 2000, true) {
                        Ok(Value::String(n)) => { logf!("[lol] connected, active player: {n}"); me = Some(n) }
                        _ => continue,
                    }
                }
                let Ok(data) = http::get_json(&format!("{base}/eventdata"), 2000, true) else { continue };
                let events = data["Events"].as_array().cloned().unwrap_or_default();
                let ids = |e: &Value| e["EventID"].as_i64().unwrap_or(-1);
                let Some(seen) = seen.as_mut() else {
                    seen = Some(events.iter().map(ids).collect()); // skip what happened before we connected
                    continue;
                };
                for e in &events {
                    if seen.insert(ids(e)) {
                        for ge in lol_translate(e, me.as_deref().unwrap_or("")) {
                            bus.emit(ge);
                        }
                    }
                }
            }
        }).ok();
        LeagueProvider { stop }
    }
}

impl Drop for LeagueProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---- Valve Game State Integration ------------------------------------------------------------
#[derive(Default)]
pub struct Cs2Tracker {
    map: Option<String>,
    stats: Option<(i64, i64, i64)>,
    round_kills: i64,
    round_hs: i64,
}

const CS2: &str = "Counter-Strike 2";

impl Cs2Tracker {
    pub fn update(&mut self, p: &Value) -> Vec<GameEvent> {
        let me = p["provider"]["steamid"].as_str();
        let player = &p["player"];
        if me.is_none() || player["steamid"].as_str() != me || !player["match_stats"].is_object() {
            return vec![]; // spectating someone else while dead
        }
        let st = &player["match_stats"];
        let n = |v: &Value| v.as_i64().unwrap_or(0);
        let cur = (n(&st["kills"]), n(&st["deaths"]), n(&st["mvps"]));
        let rk = n(&player["state"]["round_kills"]);
        let rhs = n(&player["state"]["round_killhs"]);
        let map = p["map"]["name"].as_str().map(String::from);
        let Some(prev) = self.stats.filter(|s| map == self.map && cur.0 >= s.0) else {
            (self.map, self.stats, self.round_kills, self.round_hs) = (map, Some(cur), rk, rhs);
            return vec![]; // new map / match / warmup reset: fresh baseline
        };
        let mut out = vec![];
        if rk < self.round_kills {
            self.round_kills = 0;
            self.round_hs = 0;
        }
        let new_hs = (rhs - self.round_hs).max(0);
        for i in 0..(cur.0 - prev.0).clamp(0, 5) {
            out.push(GameEvent::new(if i < new_hs { "headshot" } else { "kill" }, CS2, ""));
        }
        for k in (self.round_kills + 1)..=rk {
            let kind = match k { 3 => "triple_kill", 4 => "quad_kill", 5 => "ace", _ => continue };
            out.push(GameEvent::new(kind, CS2, format!("{k} kills this round")));
        }
        if cur.1 > prev.1 {
            out.push(GameEvent::new("death", CS2, ""));
        }
        if cur.2 > prev.2 {
            out.push(GameEvent::new("mvp", CS2, "round MVP"));
        }
        (self.stats, self.round_kills, self.round_hs) = (Some(cur), rk, rhs);
        out
    }
}

#[derive(Default)]
pub struct Dota2Tracker {
    matchid: Option<String>,
    stats: Option<(i64, i64, i64)>,
}

impl Dota2Tracker {
    pub fn update(&mut self, p: &Value) -> Vec<GameEvent> {
        let pl = &p["player"];
        if pl.get("kills").is_none() {
            return vec![];
        }
        let n = |v: &Value| v.as_i64().unwrap_or(0);
        let cur = (n(&pl["kills"]), n(&pl["deaths"]), n(&pl["assists"]));
        let m = p["map"]["matchid"].as_str().map(String::from).or_else(|| p["map"]["matchid"].as_i64().map(|x| x.to_string()));
        let Some(prev) = self.stats.filter(|_| m == self.matchid) else {
            (self.matchid, self.stats) = (m, Some(cur));
            return vec![];
        };
        let mut out = vec![];
        for _ in 0..(cur.0 - prev.0).clamp(0, 5) { out.push(GameEvent::new("kill", "Dota 2", "")); }
        if cur.1 > prev.1 { out.push(GameEvent::new("death", "Dota 2", "")); }
        for _ in 0..(cur.2 - prev.2).clamp(0, 5) { out.push(GameEvent::new("assist", "Dota 2", "")); }
        self.stats = Some(cur);
        out
    }
}

pub const GSI_PORT: u16 = 3131;
pub const GSI_TOKEN: &str = "fragtape";
const OLD_TOKEN: &str = "obsplayed"; // configs written by the OBS plugin version

#[derive(Default)]
pub struct Gsi {
    cs2: Mutex<Cs2Tracker>,
    dota: Mutex<Dota2Tracker>,
}

impl Gsi {
    pub fn handle(&self, p: &Value) -> Vec<GameEvent> {
        let token = p["auth"]["token"].as_str();
        if token != Some(GSI_TOKEN) && token != Some(OLD_TOKEN) {
            return vec![];
        }
        match p["provider"]["appid"].as_i64() {
            Some(730) => self.cs2.lock().unwrap().update(p),
            Some(570) => self.dota.lock().unwrap().update(p),
            _ => vec![],
        }
    }

    pub fn serve(self: Arc<Self>, bus: EventBus) {
        let server = match tiny_http::Server::http(("127.0.0.1", GSI_PORT)) {
            Ok(s) => s,
            Err(e) => return logf!("[gsi] could not listen on 127.0.0.1:{GSI_PORT}: {e}"),
        };
        logf!("[gsi] listening on http://127.0.0.1:{GSI_PORT}/");
        std::thread::Builder::new().name("gsi".into()).spawn(move || {
            for mut req in server.incoming_requests() {
                let mut body = Vec::new();
                let _ = std::io::Read::read_to_end(&mut std::io::Read::take(req.as_reader(), 1_000_000), &mut body);
                let _ = req.respond(tiny_http::Response::empty(200));
                if let Ok(p) = serde_json::from_slice::<Value>(&body) {
                    for e in self.handle(&p) {
                        bus.emit(e);
                    }
                }
            }
        }).ok();
    }
}

const CFG_NAME: &str = "gamestate_integration_fragtape.cfg";

fn cfg(data: &str) -> String {
    format!("\"Fragtape\"\n{{\n    \"uri\"       \"http://127.0.0.1:{GSI_PORT}/\"\n    \"timeout\"   \"5.0\"\n    \"buffer\"    \"0.0\"\n    \"throttle\"  \"0.1\"\n    \"heartbeat\" \"30.0\"\n    \"auth\" {{ \"token\" \"{GSI_TOKEN}\" }}\n    \"data\"\n    {{\n{data}    }}\n}}\n")
}

/// Write the GSI config into CS2 / Dota 2 installs. Returns what happened, per game.
pub fn install_gsi() -> Vec<String> {
    let targets: [(&str, &[&str], String); 2] = [
        ("Counter-Strike 2", &["Counter-Strike Global Offensive", "game", "csgo", "cfg"],
         cfg("        \"provider\" \"1\"\n        \"map\" \"1\"\n        \"round\" \"1\"\n        \"player_id\" \"1\"\n        \"player_state\" \"1\"\n        \"player_match_stats\" \"1\"\n")),
        ("Dota 2", &["dota 2 beta", "game", "dota", "cfg", "gamestate_integration"],
         cfg("        \"provider\" \"1\"\n        \"map\" \"1\"\n        \"player\" \"1\"\n        \"hero\" \"1\"\n")),
    ];
    let mut out = vec![];
    for (game, parts, text) in targets {
        let mut done = false;
        for lib in crate::importers::steam_libraries() {
            let game_dir = lib.join("steamapps").join("common").join(parts[0]);
            if !game_dir.is_dir() {
                continue;
            }
            let dir = parts[1..].iter().fold(game_dir, |p, s| p.join(s));
            let ok = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(CFG_NAME), &text));
            out.push(match ok {
                Ok(_) => format!("{game}: installed"),
                Err(e) => format!("{game}: {e}"),
            });
            done = true;
        }
        if !done {
            out.push(format!("{game}: not installed"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lol_events() {
        let k = lol_translate(&json!({"EventName": "ChampionKill", "KillerName": "Me#EUW", "VictimName": "X"}), "me");
        assert_eq!(k[0].kind, "kill");
        let s = lol_translate(&json!({"EventName": "BaronKill", "KillerName": "Y", "Assisters": ["Me"], "Stolen": "True"}), "Me");
        assert_eq!((s[0].kind.as_str(), s[0].detail.as_str()), ("objective_steal", "Baron Nashor"));
        assert!(lol_translate(&json!({"EventName": "ChampionKill", "KillerName": "A", "VictimName": "B"}), "Me").is_empty());
    }

    fn cs(kills: i64, rk: i64, hs: i64, deaths: i64) -> Value {
        json!({"auth": {"token": GSI_TOKEN}, "provider": {"appid": 730, "steamid": "1"}, "map": {"name": "de_dust2"},
               "player": {"steamid": "1", "match_stats": {"kills": kills, "deaths": deaths, "mvps": 0},
                          "state": {"round_kills": rk, "round_killhs": hs}}})
    }

    #[test]
    fn cs2_kills_headshots_and_multis() {
        let g = Gsi::default();
        assert!(g.handle(&cs(0, 0, 0, 0)).is_empty()); // baseline
        let e = g.handle(&cs(3, 3, 1, 0));
        let kinds: Vec<&str> = e.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["headshot", "kill", "kill", "triple_kill"]);
        assert_eq!(g.handle(&cs(3, 0, 0, 1))[0].kind, "death");
        let mut bad = cs(9, 0, 0, 0);
        bad["auth"]["token"] = json!("nope");
        assert!(g.handle(&bad).is_empty());
    }

    #[test]
    fn dota_kills() {
        let mut t = Dota2Tracker::default();
        let p = |k: i64, a: i64| json!({"map": {"matchid": "5"}, "player": {"kills": k, "deaths": 0, "assists": a}});
        assert!(t.update(&p(0, 0)).is_empty());
        assert_eq!(t.update(&p(2, 1)).len(), 3);
    }
}
