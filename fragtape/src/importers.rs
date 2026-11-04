//! Find games on this PC (Steam history + installs, Riot, Epic) to fill the library.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::events::KNOWN_GAMES;
use crate::gamedb::{is_generic, DbGame, GameDb};
use crate::win;

// ---- Valve KeyValues ----------------------------------------------------------------------
pub fn parse_vdf(text: &str) -> Value {
    let mut stack: Vec<(String, Map<String, Value>)> = vec![(String::new(), Map::new())];
    let mut key: Option<String> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                while let Some(ch) = chars.next() {
                    match ch {
                        '\\' => match chars.next() {
                            Some('"') => s.push('"'),
                            Some('\\') => s.push('\\'),
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(o) => { s.push('\\'); s.push(o); }
                            None => break,
                        },
                        '"' => break,
                        o => s.push(o),
                    }
                }
                match key.take() {
                    None => key = Some(s),
                    Some(k) => { stack.last_mut().unwrap().1.insert(k, Value::String(s)); }
                }
            }
            '{' => stack.push((key.take().unwrap_or_default(), Map::new())),
            '}' => {
                if stack.len() > 1 {
                    let (k, m) = stack.pop().unwrap();
                    stack.last_mut().unwrap().1.insert(k, Value::Object(m));
                }
                key = None;
            }
            '/' if chars.peek() == Some(&'/') => {
                for ch in chars.by_ref() {
                    if ch == '\n' { break; }
                }
            }
            _ => {}
        }
    }
    while stack.len() > 1 {
        let (k, m) = stack.pop().unwrap();
        stack.last_mut().unwrap().1.insert(k, Value::Object(m));
    }
    Value::Object(stack.pop().unwrap().1)
}

/// Case-insensitive nested lookup.
fn ci<'a>(mut v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for p in path {
        v = v.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(p))?.1;
    }
    Some(v)
}

pub fn steam_libraries() -> Vec<PathBuf> {
    let mut roots = vec![];
    for (hive, key) in [(win::HKCU, r"Software\Valve\Steam"), (win::HKLM, r"SOFTWARE\WOW6432Node\Valve\Steam")] {
        for value in ["SteamPath", "InstallPath"] {
            if let Some(p) = win::reg_string(hive, key, value) {
                roots.push(PathBuf::from(p));
            }
        }
    }
    roots.push(PathBuf::from(std::env::var("ProgramFiles(x86)").unwrap_or(r"C:\Program Files (x86)".into())).join("Steam"));
    let mut libs = vec![];
    for root in roots {
        if let Ok(text) = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
            let v = parse_vdf(&text);
            if let Some(folders) = ci(&v, &["libraryfolders"]).and_then(|f| f.as_object()) {
                for f in folders.values() {
                    if let Some(p) = f.get("path").and_then(|p| p.as_str()) {
                        libs.push(PathBuf::from(p));
                    }
                }
            }
        }
        libs.push(root);
    }
    let mut seen = HashSet::new();
    libs.into_iter().filter(|l| l.join("steamapps").is_dir() && seen.insert(l.to_string_lossy().to_lowercase().replace('/', "\\"))).collect()
}

fn not_game_exe(rel: &str) -> bool {
    let dirs: Vec<&str> = rel.split('/').collect();
    let bad_dir = dirs[..dirs.len().saturating_sub(1)].iter().any(|d| {
        matches!(*d, "support" | "_commonredist" | "commonredist" | "installer" | "installers" | "directx" | "vcredist"
            | "easyanticheat" | "battleye" | "tool" | "tools") || d.starts_with("redist")
    });
    bad_dir || ["setup", "install", "crash", "cleaner", "uninst", "report", "updater", "helper"].iter().any(|w| rel.contains(w))
}

fn not_a_game(name: &str) -> bool {
    let n = name.to_lowercase();
    ["redistributable", "steamworks", "soundtrack", "dedicated server", "benchmark", "steamvr"].iter().any(|w| n.contains(w))
        || n.split(|c: char| !c.is_alphanumeric()).any(|w| w == "sdk")
}

fn builtin_for(exes: &[String], name: &str) -> Option<&'static str> {
    let bases: HashSet<&str> = exes.iter().map(|e| e.rsplit('/').next().unwrap_or(e)).collect();
    KNOWN_GAMES.iter().find(|(_, ex, _)| ex.iter().any(|e| bases.contains(e)))
        .or_else(|| KNOWN_GAMES.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name)))
        .map(|(n, _, _)| *n)
}

fn make_entry(db_game: Option<&DbGame>, name: &str, source: &str, info: Value) -> Map<String, Value> {
    let exes: Vec<String> = db_game.map(|g| g.exes.clone()).unwrap_or_default();
    let builtin = builtin_for(&exes, name);
    let mut out = Map::new();
    out.insert("name".into(), json!(builtin.map(String::from).or(db_game.map(|g| g.name.clone())).unwrap_or(name.into())));
    out.insert("source".into(), json!(source));
    out.insert("exes".into(), json!(exes.iter().filter(|e| e.contains('/') || !is_generic(e)).collect::<Vec<_>>()));
    for (k, v) in info.as_object().cloned().unwrap_or_default() {
        if !v.is_null() {
            out.insert(k, v);
        }
    }
    if let Some(g) = db_game {
        out.insert("discord_id".into(), json!(g.id));
        out.insert("icon".into(), json!(g.icon));
    }
    out
}

/// Known game exes in an install folder, as "<folder>/<relative path>" (exact detection).
pub fn game_exes_in(folder: &Path, db: &GameDb) -> Vec<String> {
    let known: HashSet<&str> = KNOWN_GAMES.iter().flat_map(|(_, e, _)| e.iter().copied()).collect();
    let top = folder.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut out = vec![];
    let mut level = vec![(folder.to_path_buf(), String::new())];
    for depth in 0..3 {
        let mut next = vec![];
        for (path, rel) in &level {
            let Ok(rd) = fs::read_dir(path) else { continue };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                let ft = entry.file_type();
                if ft.as_ref().is_ok_and(|t| t.is_dir()) && depth < 2 {
                    next.push((entry.path(), format!("{rel}{name}/")));
                } else if name.ends_with(".exe") && !is_generic(&name) && !not_game_exe(&format!("{rel}{name}"))
                    && (known.contains(name.as_str()) || db.for_exe(&name).is_some())
                {
                    out.push(format!("{top}/{rel}{name}"));
                }
            }
        }
        level = next;
    }
    out
}

pub fn steam_games(db: &GameDb, libraries: &[PathBuf]) -> Vec<Map<String, Value>> {
    let mut played: HashMap<String, (i64, i64)> = HashMap::new();
    let mut installed: HashMap<String, String> = HashMap::new();
    let mut folders: HashMap<String, PathBuf> = HashMap::new();
    for lib in libraries {
        let apps = lib.join("steamapps");
        for e in fs::read_dir(&apps).into_iter().flatten().flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if !(fname.starts_with("appmanifest_") && fname.ends_with(".acf")) {
                continue;
            }
            let Ok(text) = fs::read(e.path()) else { continue };
            let v = parse_vdf(&String::from_utf8_lossy(&text));
            let Some(st) = ci(&v, &["AppState"]) else { continue };
            if let (Some(id), Some(name)) = (st["appid"].as_str(), st["name"].as_str()) {
                installed.insert(id.into(), name.into());
                if let Some(dir) = st["installdir"].as_str() {
                    folders.insert(id.into(), apps.join("common").join(dir));
                }
            }
        }
        for user in fs::read_dir(lib.join("userdata")).into_iter().flatten().flatten() {
            let Ok(text) = fs::read(user.path().join("config").join("localconfig.vdf")) else { continue };
            let v = parse_vdf(&String::from_utf8_lossy(&text));
            let Some(apps) = ci(&v, &["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"]).and_then(|a| a.as_object()) else { continue };
            for (id, a) in apps {
                let num = |k: &str| a.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let (last, minutes) = (num("LastPlayed"), num("Playtime"));
                if last > 0 || minutes > 0 {
                    let p = played.entry(id.clone()).or_insert((0, 0));
                    *p = (p.0.max(last), p.1.max(minutes));
                }
            }
        }
    }
    let ids: HashSet<&String> = played.keys().chain(installed.keys()).collect();
    let mut out = vec![];
    for id in ids {
        let name = installed.get(id);
        if name.is_some_and(|n| not_a_game(n)) {
            continue;
        }
        let game = db.by_sku("steam", id);
        let Some(display) = name.cloned().or(game.as_ref().map(|g| g.name.clone())) else { continue };
        let (last, minutes) = played.get(id).copied().unwrap_or((0, 0));
        let mut e = make_entry(game.as_deref(), &display, "steam", json!({
            "steam_appid": id, "installed": installed.contains_key(id),
            "last_played": if last > 0 { json!(last) } else { Value::Null }, "playtime": minutes * 60,
        }));
        if let Some(folder) = folders.get(id) {
            let mut exes: Vec<String> = e["exes"].as_array().unwrap().iter().filter_map(|x| x.as_str().map(String::from)).collect();
            exes.extend(game_exes_in(folder, db));
            exes.sort();
            exes.dedup();
            e.insert("exes".into(), json!(exes));
            if let Some(n) = name {
                if !KNOWN_GAMES.iter().any(|(k, _, _)| *k == e["name"].as_str().unwrap_or("")) {
                    e.insert("name".into(), json!(n.replace(['®', '™'], "").trim()));
                }
            }
        }
        out.push(e);
    }
    out
}

fn program_data() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or(r"C:\ProgramData".into()))
}

pub fn riot_games(db: &GameDb) -> Vec<Map<String, Value>> {
    let Some(data) = crate::util::read_json(&program_data().join("Riot Games").join("RiotClientInstalls.json")) else { return vec![] };
    let mut out = vec![];
    for folder in data["associated_client"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        let low = folder.to_lowercase();
        let games: [(&str, &str, &str); 3] = [
            ("league of legends", "League of Legends", "Game/League of Legends.exe"),
            ("valorant", "VALORANT", "ShooterGame/Binaries/Win64/VALORANT-Win64-Shipping.exe"),
            ("legends of runeterra", "Legends of Runeterra", "LoR.exe"),
        ];
        for (key, name, exe) in games {
            if low.contains(key) {
                // associated_client keys look like "C:/Riot Games/VALORANT/live/"
                let exe_path = Path::new(&folder).join(exe);
                let info = json!({"installed": Path::new(&folder).exists(),
                                  "exe_path": exe_path.is_file().then(|| exe_path.to_string_lossy().to_string())});
                out.push(make_entry(db.by_name(name).as_deref(), name, "riot", info));
            }
        }
    }
    out
}

pub fn epic_games(db: &GameDb) -> Vec<Map<String, Value>> {
    let dir = program_data().join("Epic").join("EpicGamesLauncher").join("Data").join("Manifests");
    let mut out = vec![];
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_none_or(|x| x != "item") {
            continue;
        }
        let Some(m) = crate::util::read_json(&e.path()) else { continue };
        let Some(name) = m["DisplayName"].as_str() else { continue };
        if not_a_game(name) {
            continue;
        }
        let exe = m["LaunchExecutable"].as_str().unwrap_or("").replace('\\', "/").rsplit('/').next().unwrap_or("").to_lowercase();
        let game = db.by_name(name).or_else(|| (!exe.is_empty() && !is_generic(&exe)).then(|| db.for_exe(&exe)).flatten());
        let exe_path = match (m["InstallLocation"].as_str(), m["LaunchExecutable"].as_str()) {
            (Some(dir), Some(exe)) if !exe.is_empty() => Some(Path::new(dir).join(exe).to_string_lossy().to_string()),
            _ => None,
        };
        let mut entry = make_entry(game.as_deref(), name, "epic", json!({"installed": true, "exe_path": exe_path}));
        if !exe.is_empty() && !is_generic(&exe) {
            let exes = entry["exes"].as_array_mut().unwrap();
            if !exes.iter().any(|x| x == &json!(exe)) {
                exes.push(json!(exe));
            }
        }
        out.push(entry);
    }
    out
}

/// Combine entries with the same name (a game and its test server, ...).
pub fn merge(entries: Vec<Map<String, Value>>) -> Vec<Value> {
    let mut found: Vec<Map<String, Value>> = vec![];
    let mut pos: HashMap<String, usize> = HashMap::new();
    for e in entries {
        let key = e["name"].as_str().unwrap_or("").to_lowercase();
        let Some(&i) = pos.get(&key) else {
            pos.insert(key, found.len());
            found.push(e);
            continue;
        };
        let f = &mut found[i];
        let mut exes: Vec<Value> = f["exes"].as_array().cloned().unwrap_or_default();
        exes.extend(e["exes"].as_array().cloned().unwrap_or_default());
        exes.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        exes.dedup();
        f.insert("exes".into(), json!(exes));
        let inst = |m: &Map<String, Value>| m.get("installed").and_then(|v| v.as_bool()).unwrap_or(false);
        let num = |m: &Map<String, Value>, k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let installed_e = inst(&e);
        f.insert("installed".into(), json!(inst(f) || installed_e));
        let last = num(f, "last_played").max(num(&e, "last_played"));
        f.insert("last_played".into(), if last > 0 { json!(last) } else { Value::Null });
        f.insert("playtime".into(), json!(num(f, "playtime").max(num(&e, "playtime"))));
        if installed_e {
            for k in ["steam_appid", "discord_id", "icon", "exe_path"] {
                if let Some(v) = e.get(k).filter(|v| !v.is_null()) {
                    f.insert(k.into(), v.clone());
                }
            }
        }
    }
    found.into_iter().map(Value::Object).collect()
}

/// Cheap fingerprint of the files a scan reads: unchanged = nothing new to find.
pub fn signature() -> String {
    let mut files = vec![];
    for lib in steam_libraries() {
        files.push(lib.join("steamapps"));
        for u in fs::read_dir(lib.join("userdata")).into_iter().flatten().flatten() {
            files.push(u.path().join("config").join("localconfig.vdf"));
        }
    }
    let pd = program_data();
    files.push(pd.join("Riot Games").join("RiotClientInstalls.json"));
    files.push(pd.join("Epic").join("EpicGamesLauncher").join("Data").join("Manifests"));
    let mut parts: Vec<String> = files.iter().filter_map(|f| {
        let m = fs::metadata(f).ok()?;
        let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        Some(format!("{}:{t}:{}", f.display(), m.len()))
    }).collect();
    parts.sort();
    parts.join("|")
}

pub fn scan(db: &GameDb) -> Vec<Value> {
    let mut all = steam_games(db, &steam_libraries());
    all.extend(riot_games(db));
    all.extend(epic_games(db));
    merge(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vdf() {
        let v = parse_vdf(r#""AppState" { "appid" "730" "name" "Counter-Strike 2" "x" { "y" "a \"q\"" } } // c"#);
        assert_eq!(ci(&v, &["appstate", "APPID"]).unwrap(), "730");
        assert_eq!(v["AppState"]["x"]["y"], "a \"q\"");
    }

    #[test]
    fn filters() {
        assert!(not_game_exe("_commonredist/vcredist_x64.exe"));
        assert!(not_game_exe("bin/crashreporter.exe"));
        assert!(!not_game_exe("bin/win64/game-shipping.exe"));
        assert!(not_a_game("Steamworks Common Redistributables"));
        assert!(not_a_game("Source SDK"));
        assert!(!not_a_game("Sdkfoo Racing"));
    }

    #[test]
    fn merge_combines_by_name() {
        let a = json!({"name": "R6", "exes": ["a.exe"], "installed": false, "playtime": 10}).as_object().unwrap().clone();
        let b = json!({"name": "r6", "exes": ["b.exe"], "installed": true, "steam_appid": "2", "playtime": 5}).as_object().unwrap().clone();
        let m = merge(vec![a, b]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["exes"], json!(["a.exe", "b.exe"]));
        assert_eq!((m[0]["installed"].clone(), m[0]["steam_appid"].clone(), m[0]["playtime"].clone()), (json!(true), json!("2"), json!(10)));
    }
}
