//! Discord's public list of detectable games (~10k Windows games with their exe names).
//! Downloaded once in the background, cached as gamedb.json, refreshed weekly.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};

use crate::win::Snapshot;
use crate::{http, logf, util};

const URL: &str = "https://discord.com/api/v9/applications/detectable";
const MAX_AGE_SECS: u64 = 7 * 24 * 3600;
const CACHE_VERSION: i64 = 2;

/// Exe names many unrelated programs share: they only match together with a folder.
pub const GENERIC_EXES: &[&str] = &[
    "game.exe", "launcher.exe", "client.exe", "engine.exe", "start.exe", "play.exe", "setup.exe", "javaw.exe",
    "java.exe", "nw.exe", "electron.exe", "unity.exe", "ue4game.exe", "main.exe", "app.exe", "run.exe", "autorun.exe",
    "dosbox.exe", "hl2.exe", "python.exe", "pythonw.exe", "chrome.exe", "msedge.exe", "firefox.exe", "explorer.exe",
    "steam.exe", "obs64.exe", "aseprite.exe", "fragtape.exe",
];

pub fn is_generic(exe: &str) -> bool {
    GENERIC_EXES.contains(&exe)
}

#[derive(Clone, Debug)]
pub struct DbGame {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub exes: Vec<String>, // lower-case, "/" separators, may include parent folders
    pub skus: Vec<String>, // "steam:730", ...
}

impl DbGame {
    pub fn icon_url(&self) -> Option<String> {
        self.icon.as_ref().map(|i| format!("https://cdn.discordapp.com/app-icons/{}/{}.png?size=64", self.id, i))
    }
}

/// Keep only what we need from Discord's payload (13 MB -> ~1 MB).
pub fn slim(raw: &Value) -> Vec<Value> {
    let mut out = vec![];
    for app in raw.as_array().into_iter().flatten() {
        let exes: Vec<String> = app["executables"].as_array().into_iter().flatten()
            .filter(|e| e["os"] == "win32" && !e["is_launcher"].as_bool().unwrap_or(false))
            .filter_map(|e| e["name"].as_str())
            .filter(|n| !n.is_empty() && !n.starts_with('>'))
            .map(|n| n.to_lowercase().replace('\\', "/"))
            .collect();
        let Some(name) = app["name"].as_str() else { continue };
        if exes.is_empty() {
            continue;
        }
        let mut skus: Vec<String> = app["third_party_skus"].as_array().into_iter().flatten()
            .filter_map(|s| Some(format!("{}:{}", s["distributor"].as_str()?, s["id"].as_str()?).to_lowercase()))
            .collect();
        skus.sort();
        skus.dedup();
        let id = app["id"].as_str().map(String::from).or_else(|| app["id"].as_u64().map(|n| n.to_string()));
        out.push(json!({"id": id, "name": name, "icon": app["icon_hash"].as_str().or(app["icon"].as_str()),
                        "exes": exes, "skus": skus}));
    }
    out
}

#[derive(Default)]
struct Index {
    games: Vec<Arc<DbGame>>,
    by_exe: HashMap<String, Vec<(String, Arc<DbGame>)>>,
    by_id: HashMap<String, Arc<DbGame>>,
    by_name: HashMap<String, Arc<DbGame>>,
    by_sku: HashMap<String, Arc<DbGame>>,
}

pub struct GameDb {
    path: PathBuf,
    index: RwLock<Index>,
}

impl GameDb {
    pub fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new(GameDb { path: dir.join("gamedb.json"), index: RwLock::new(Index::default()) })
    }

    pub fn ready(&self) -> bool {
        !self.index.read().unwrap().games.is_empty()
    }

    pub fn count(&self) -> usize {
        self.index.read().unwrap().games.len()
    }

    pub fn load_list(&self, data: &[Value]) {
        let games: Vec<Arc<DbGame>> = data.iter().filter_map(|d| {
            Some(Arc::new(DbGame {
                id: d["id"].as_str()?.to_string(),
                name: d["name"].as_str()?.to_string(),
                icon: d["icon"].as_str().map(String::from),
                exes: d["exes"].as_array()?.iter().filter_map(|e| e.as_str().map(String::from)).collect(),
                skus: d["skus"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect(),
            }))
        }).collect();
        let mut idx = Index::default();
        for g in &games {
            for exe in &g.exes {
                let base = exe.rsplit('/').next().unwrap_or(exe).to_string();
                idx.by_exe.entry(base).or_default().push((exe.clone(), g.clone()));
            }
            idx.by_id.insert(g.id.clone(), g.clone());
            idx.by_name.insert(g.name.to_lowercase(), g.clone());
            for s in &g.skus {
                idx.by_sku.insert(s.clone(), g.clone());
            }
        }
        idx.games = games;
        *self.index.write().unwrap() = idx;
    }

    fn load_cached(&self) -> bool {
        match util::read_json(&self.path) {
            Some(v) if v["v"].as_i64() == Some(CACHE_VERSION) => {
                self.load_list(v["games"].as_array().map(|a| a.as_slice()).unwrap_or(&[]));
                true
            }
            _ => false,
        }
    }

    fn stale(&self) -> bool {
        std::fs::metadata(&self.path).and_then(|m| m.modified()).ok()
            .and_then(|t| t.elapsed().ok()).map_or(true, |age| age.as_secs() > MAX_AGE_SECS)
    }

    fn refresh(&self) {
        match http::get_json(URL, 60_000, false) {
            Ok(raw) => {
                let data = slim(&raw);
                let _ = util::write_atomic(&self.path, &serde_json::to_vec(&json!({"v": CACHE_VERSION, "games": data})).unwrap());
                self.load_list(&data);
                logf!("[gamedb] downloaded {} games", self.count());
            }
            Err(e) => logf!("[gamedb] download failed: {e}"),
        }
    }

    /// Load the cache now; download in the background if missing or old.
    pub fn start(self: &Arc<Self>, on_ready: impl FnOnce() + Send + 'static) {
        if self.load_cached() {
            logf!("[gamedb] {} games loaded", self.count());
        }
        let need = !self.ready() || self.stale();
        let me = self.clone();
        std::thread::Builder::new().name("gamedb".into()).spawn(move || {
            crate::win::lower_thread_priority();
            if need {
                me.refresh();
            }
            if me.ready() {
                on_ready();
            }
        }).ok();
    }

    // ---- queries -------------------------------------------------------------------------
    pub fn get(&self, id: &str) -> Option<Arc<DbGame>> {
        self.index.read().unwrap().by_id.get(id).cloned()
    }

    pub fn by_sku(&self, store: &str, id: &str) -> Option<Arc<DbGame>> {
        self.index.read().unwrap().by_sku.get(&format!("{store}:{id}").to_lowercase()).cloned()
    }

    pub fn by_name(&self, name: &str) -> Option<Arc<DbGame>> {
        self.index.read().unwrap().by_name.get(&name.to_lowercase()).cloned()
    }

    pub fn for_exe(&self, exe: &str) -> Option<Arc<DbGame>> {
        self.index.read().unwrap().by_exe.get(&exe.to_lowercase()).and_then(|c| c.first().map(|(_, g)| g.clone()))
    }

    /// The running game, if Discord knows it.
    pub fn find_running(&self, running: &Snapshot) -> Option<Arc<DbGame>> {
        let idx = self.index.read().unwrap();
        for name in running.names() {
            let Some(candidates) = idx.by_exe.get(name) else { continue };
            let mut paths: Option<Vec<String>> = None;
            let distinct: HashSet<&str> = candidates.iter().map(|(_, g)| g.id.as_str()).collect();
            for (full, game) in candidates {
                if full.contains('/') {
                    let p = paths.get_or_insert_with(|| running.paths(name));
                    if p.iter().any(|p| p.ends_with(&format!("/{full}"))) {
                        return Some(game.clone());
                    }
                } else if !is_generic(name) && distinct.len() == 1 {
                    return Some(game.clone());
                }
            }
        }
        None
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<Arc<DbGame>> {
        let q = query.trim().to_lowercase();
        if q.chars().count() < 2 {
            return vec![];
        }
        let idx = self.index.read().unwrap();
        let (mut starts, mut contains): (Vec<_>, Vec<_>) = (vec![], vec![]);
        for g in &idx.games {
            let n = g.name.to_lowercase();
            if n.starts_with(&q) {
                starts.push(g.clone());
            } else if n.contains(&q) {
                contains.push(g.clone());
            }
        }
        starts.sort_by_key(|g| g.name.len());
        contains.sort_by_key(|g| g.name.len());
        starts.into_iter().chain(contains).take(limit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Arc<GameDb> {
        let db = GameDb::new(std::env::temp_dir());
        let raw = json!([
            {"id": "1", "name": "Elden Ring", "icon_hash": "abc", "executables": [{"os": "win32", "name": "elden ring/game/eldenring.exe"}, {"os": "win32", "name": "game.exe"}],
             "third_party_skus": [{"distributor": "steam", "id": "1245620"}]},
            {"id": "2", "name": "Hades", "executables": [{"os": "win32", "name": "hades.exe"}, {"os": "darwin", "name": "hades"}]},
            {"id": "3", "name": "Launcher Only", "executables": [{"os": "win32", "name": "l.exe", "is_launcher": true}]},
        ]);
        db.load_list(&slim(&raw));
        db
    }

    #[test]
    fn slim_and_lookups() {
        let db = db();
        assert_eq!(db.count(), 2);
        assert_eq!(db.by_sku("steam", "1245620").unwrap().name, "Elden Ring");
        assert_eq!(db.get("2").unwrap().exes, vec!["hades.exe"]);
        assert!(db.get("1").unwrap().icon_url().unwrap().contains("/1/abc.png"));
        assert_eq!(db.search("ha", 10)[0].name, "Hades");
    }

    #[test]
    fn matching_needs_folder_for_generic_names() {
        let db = db();
        assert_eq!(db.find_running(&Snapshot::from_names(&["hades.exe"])).unwrap().name, "Hades");
        assert!(db.find_running(&Snapshot::from_names(&["game.exe"])).is_none());
        let s = Snapshot::with_paths(&[("eldenring.exe", "c:/steam/common/elden ring/game/eldenring.exe")]);
        assert_eq!(db.find_running(&s).unwrap().name, "Elden Ring");
    }
}
