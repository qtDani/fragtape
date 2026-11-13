//! The window's local web server (127.0.0.1 only).
//!
//! Security: checks the Host header (DNS rebinding) and requires a per-install
//! secret token, sent as a header for API calls (or ?t= for <video>/<img> URLs,
//! which can't send headers). Cross-site pages can't add custom headers without
//! a CORS preflight, which this server never approves.

use std::io::Read;
use std::sync::Arc;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::app::{Cmd, Shared};
use crate::events::has_kill_detection;
use crate::library::MODES;
use crate::logf;


fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn query(url: &str) -> Vec<(String, String)> {
    url.split_once('?').map(|(_, q)| q).unwrap_or("").split('&').filter_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        Some((decode(k), decode(v)))
    }).collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn q<'a>(qs: &'a [(String, String)], k: &str) -> &'a str {
    qs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).unwrap_or("")
}

fn json_resp(code: u16, v: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(v.to_string().into_bytes()).with_status_code(code)
        .with_header(header("Content-Type", "application/json")).with_header(header("Cache-Control", "no-store"))
}

pub fn start(shared: Arc<Shared>) {
    let server = match Server::http(("127.0.0.1", shared.port)) {
        Ok(s) => s,
        Err(e) => return logf!("[window] could not listen on 127.0.0.1:{}: {e}", shared.port),
    };
    logf!("[window] http://127.0.0.1:{}/", shared.port);
    std::thread::Builder::new().name("http".into()).spawn(move || {
        for req in server.incoming_requests() {
            let s = shared.clone();
            // One thread per request: a video stream must not block the API.
            std::thread::spawn(move || handle(&s, req));
        }
    }).ok();
}

fn handle(s: &Shared, mut req: Request) {
    let host = req.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.to_string()).unwrap_or_default();
    if host != format!("127.0.0.1:{}", s.port) && host != format!("localhost:{}", s.port) {
        let _ = req.respond(json_resp(403, &json!({"error": "forbidden"})));
        return;
    }
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let qs = query(&url);
    if !req.headers().iter().any(|h| h.field.equiv("X-Fragtape-Token") && h.value.as_str() == s.token) {
        let _ = req.respond(json_resp(403, &json!({"error": "forbidden"})));
        return;
    }
    if *req.method() == Method::Get {
        let (code, body) = match path.as_str() {
            "/api/state" => (200, crate::app::state(s)),
            "/api/clips" => (200, json!({"clips": s.clips.list(s.active_path.lock().unwrap().as_deref())})),
            "/api/search" => (200, search(s, q(&qs, "q"))),
            _ => (404, json!({"error": "not found"})),
        };
        let _ = req.respond(json_resp(code, &body));
        return;
    }
    if *req.method() != Method::Post {
        let _ = req.respond(json_resp(405, &json!({"error": "method"})));
        return;
    }
    let mut body = Vec::new();
    let _ = req.as_reader().take(100_000).read_to_end(&mut body);
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let result = post(s, &path, &v);
    let resp = match result {
        Ok(v) => json_resp(200, &v),
        Err(e) if e == "not found" => json_resp(404, &json!({"error": e})),
        Err(e) => json_resp(400, &json!({"error": e})),
    };
    let _ = req.respond(resp);
}

/// Games from the game list matching `query` (for the native window).
pub fn search_games(s: &Shared, query: &str) -> Vec<Value> {
    search(s, query).as_array().cloned().unwrap_or_default()
}

fn search(s: &Shared, query: &str) -> Value {
    let have: std::collections::HashSet<String> = s.library.games().iter()
        .filter_map(|g| g["name"].as_str().map(|n| n.to_lowercase())).collect();
    Value::Array(s.gamedb.search(query, 20).iter().map(|g| json!({
        "id": g.id, "name": g.name, "icon": g.icon.as_ref().map(|_| format!("/icon/db/{}?t={}", g.id, s.token)),
        "added": have.contains(&g.name.to_lowercase()),
        "kills": has_kill_detection(&g.name),
    })).collect())
}

pub fn post(s: &Shared, path: &str, b: &Value) -> Result<Value, String> {
    let str_of = |k: &str| b[k].as_str().map(String::from).ok_or(format!("{k} required"));
    let lib = &s.library;
    match path {
        "/api/mode" => lib.set_mode(&str_of("name")?, &str_of("mode")?)?,
        "/api/default" => lib.set_default_mode(&str_of("mode")?)?,
        "/api/remove" => lib.remove(&str_of("name")?),
        "/api/scan" => crate::app::scan_async(s, true),
        "/api/pref" => {
            let key = str_of("key")?;
            lib.set_pref(&key, &b["value"])?;
            if matches!(key.as_str(), "theme" | "accent" | "card_corner" | "card_time" | "animations") {
                crate::toast::set_look(&lib.prefs()); // right away, so a test card already uses it
            }
            s.send(Cmd::PrefChanged(key));
        }
        "/api/reset-advanced" => {
            lib.reset_advanced();
            s.send(Cmd::PrefChanged("quality".into()));
        }
        "/api/answer" => {
            let mode = str_of("mode")?;
            s.send(Cmd::Answer(str_of("name")?, MODES.contains(&mode.as_str()).then_some(mode)));
        }
        "/api/add" => {
            let mode = b["mode"].as_str().unwrap_or("ask").to_string();
            if let Some(id) = b["id"].as_str() {
                let g = s.gamedb.get(id).ok_or("unknown game")?;
                lib.add(&g.name, json!({"source": "discord", "discord_id": g.id, "icon": g.icon, "exes": g.exes}));
                lib.set_mode(&g.name, &mode)?;
            } else {
                let mut exe = str_of("exe")?.trim().to_lowercase();
                if exe.is_empty() {
                    return Err("exe required".into());
                }
                if !exe.ends_with(".exe") {
                    exe.push_str(".exe");
                }
                let name = b["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(String::from)
                    .unwrap_or_else(|| exe.trim_end_matches(".exe").to_string());
                lib.add(&name, json!({"source": "custom", "exes": [exe]}));
                lib.set_mode(&name, &mode)?;
            }
        }
        "/api/clip/update" => s.clips.update(&str_of("id")?, b)?,
        "/api/clip/delete" => s.clips.delete(&str_of("id")?)?,
        "/api/clip/trim" => {
            let name = s.clips.trim(&str_of("id")?, b["start"].as_f64().ok_or("start")?, b["end"].as_f64().ok_or("end")?,
                                    crate::clips::find_ffmpeg().as_deref(), b["title"].as_str().unwrap_or(""))?;
            return Ok(json!({"ok": true, "name": name}));
        }
        "/api/clip/reveal" => {
            let p = s.clips.path_for(&str_of("id")?).ok_or("not found")?;
            let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
        }
        "/api/open-folder" => {
            let _ = std::fs::create_dir_all(&s.clips.root);
            let _ = std::process::Command::new("explorer").arg(&s.clips.root).spawn();
        }
        "/api/install-ffmpeg" => crate::app::install_ffmpeg(s),
        "/api/test-card" => {
            std::thread::spawn(|| crate::toast::info("TEST", "This is where cards show up",
                "In a game they sit inside the game's window.", "Fragtape", None, vec![]));
        }
        "/api/obs/install" => crate::app::install_obs(s),
        "/api/obs/download" => crate::app::open_url(crate::app::OBS_DOWNLOAD),
        "/api/obs/browse" => {
            let picked = crate::app::browse_obs(s)?;
            return Ok(json!({"ok": true, "picked": picked.is_some(), "path": s.engine.obs_dir()}));
        }
        "/api/obs/path" => {
            let using = crate::app::set_obs_path(s, b["path"].as_str())?;
            return Ok(json!({"ok": true, "path": using}));
        }
        "/api/install-gsi" => return Ok(json!({"ok": true, "results": crate::providers::install_gsi()})),
        "/api/mark" => s.send(Cmd::Mark),
        "/api/screen" => s.send(Cmd::ScreenRec),
        "/api/stop" => s.send(Cmd::Stop),
        "/api/show" => s.send(Cmd::Show),
        "/api/quit" => s.send(Cmd::Quit),
        _ => return Err("not found".into()),
    }
    Ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decoding() {
        let qs = query("/api/search?q=counter%20strike+2&t=abc");
        assert_eq!(q(&qs, "q"), "counter strike 2");
        assert_eq!(q(&qs, "t"), "abc");
        assert_eq!(q(&qs, "missing"), "");
    }
}
