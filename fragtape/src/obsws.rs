//! Minimal obs-websocket v5 client: authenticate, send requests, receive events.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tungstenite::{client::IntoClientRequest, Message, WebSocket};

const SUB_OUTPUTS: u64 = 1 << 6;

pub fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let secret = b64.encode(Sha256::digest(format!("{password}{salt}")));
    b64.encode(Sha256::digest(format!("{secret}{challenge}")))
}

type Pending = Arc<Mutex<HashMap<String, Sender<Value>>>>;

pub struct ObsWs {
    ws: Arc<Mutex<WebSocket<TcpStream>>>,
    pending: Pending,
    ids: AtomicU64,
    pub closed: Arc<AtomicBool>,
}

impl ObsWs {
    /// Connect and identify. `on_event(type, data)` runs on the reader thread;
    /// `on_close` runs once when the connection ends.
    pub fn connect(port: u16, password: &str, on_event: impl Fn(&str, &Value) + Send + 'static,
                   on_close: impl FnOnce() + Send + 'static) -> Result<Self, String> {
        let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(3)).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut req = format!("ws://127.0.0.1:{port}").into_client_request().map_err(|e| e.to_string())?;
        req.headers_mut().insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().unwrap());
        let (mut ws, _) = tungstenite::client(req, stream).map_err(|e| e.to_string())?;
        let hello = read_json(&mut ws)?;
        if hello["op"] != 0 {
            return Err("expected Hello".into());
        }
        let mut identify = json!({"rpcVersion": 1, "eventSubscriptions": SUB_OUTPUTS});
        let auth = &hello["d"]["authentication"];
        if auth.is_object() {
            identify["authentication"] = json!(auth_string(password, auth["salt"].as_str().unwrap_or(""), auth["challenge"].as_str().unwrap_or("")));
        }
        ws.send(Message::text(json!({"op": 1, "d": identify}).to_string())).map_err(|e| e.to_string())?;
        if read_json(&mut ws)?["op"] != 2 {
            return Err("authentication failed".into());
        }
        // Short read timeouts let request senders take the socket between reads.
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(50))).ok();
        let ws = Arc::new(Mutex::new(ws));
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let (ws2, pending2, closed2) = (ws.clone(), pending.clone(), closed.clone());
        std::thread::Builder::new().name("obsws".into()).spawn(move || {
            loop {
                if closed2.load(Ordering::Relaxed) {
                    break;
                }
                let msg = ws2.lock().unwrap().read();
                match msg {
                    Ok(Message::Text(t)) => {
                        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                        match v["op"].as_i64() {
                            Some(7) => {
                                let id = v["d"]["requestId"].as_str().unwrap_or("").to_string();
                                if let Some(tx) = pending2.lock().unwrap().remove(&id) {
                                    let _ = tx.send(v["d"].clone());
                                }
                            }
                            Some(5) => on_event(v["d"]["eventType"].as_str().unwrap_or(""), &v["d"]["eventData"]),
                            _ => {}
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            closed2.store(true, Ordering::Relaxed);
            pending2.lock().unwrap().clear(); // wakes waiting requests with an error
            on_close();
        }).map_err(|e| e.to_string())?;
        Ok(ObsWs { ws, pending, ids: AtomicU64::new(1), closed })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn request(&self, kind: &str, data: Option<Value>, timeout: Duration) -> Result<Value, String> {
        if self.is_closed() {
            return Err("not connected".into());
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let mut d = json!({"requestType": kind, "requestId": id});
        if let Some(data) = data {
            d["requestData"] = data;
        }
        let sent = self.ws.lock().unwrap().send(Message::text(json!({"op": 6, "d": d}).to_string()));
        if let Err(e) = sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(e.to_string());
        }
        let resp = rx.recv_timeout(timeout).map_err(|_| {
            self.pending.lock().unwrap().remove(&id);
            if self.is_closed() { format!("{kind}: connection closed") } else { format!("{kind}: timed out") }
        })?;
        let st = &resp["requestStatus"];
        if st["result"] != true {
            return Err(format!("{kind}: {}", st["comment"].as_str().map(String::from).unwrap_or_else(|| st["code"].to_string())));
        }
        Ok(resp["responseData"].clone())
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            let _ = self.ws.lock().unwrap().close(None);
        }
    }
}

impl Drop for ObsWs {
    fn drop(&mut self) {
        self.close();
    }
}

fn read_json(ws: &mut WebSocket<TcpStream>) -> Result<Value, String> {
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
            Message::Close(_) => return Err("closed by OBS".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_matches_spec() {
        // base64(sha256(base64(sha256(password + salt)) + challenge))
        let b64 = base64::engine::general_purpose::STANDARD;
        let secret = b64.encode(Sha256::digest(b"pwsalt"));
        let expected = b64.encode(Sha256::digest(format!("{secret}chal")));
        assert_eq!(auth_string("pw", "salt", "chal"), expected);
    }

    #[test]
    fn roundtrip_with_fake_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept_hdr(s, |_req: &tungstenite::handshake::server::Request, mut resp: tungstenite::handshake::server::Response| {
                resp.headers_mut().insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().unwrap());
                Ok(resp)
            }).unwrap();
            ws.send(Message::text(json!({"op": 0, "d": {"rpcVersion": 1, "authentication": {"salt": "s", "challenge": "c"}}}).to_string())).unwrap();
            let ident: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            if ident["d"]["authentication"] != auth_string("pw", "s", "c") {
                return;
            }
            ws.send(Message::text(json!({"op": 2, "d": {}}).to_string())).unwrap();
            ws.send(Message::text(json!({"op": 5, "d": {"eventType": "RecordStateChanged", "eventData": {"x": 1}}}).to_string())).unwrap();
            while let Ok(Message::Text(t)) = ws.read() {
                let d: Value = serde_json::from_str(&t).unwrap();
                let ok = d["d"]["requestType"] != "Fail";
                ws.send(Message::text(json!({"op": 7, "d": {"requestId": d["d"]["requestId"], "requestStatus": {"result": ok, "code": 600, "comment": "nope"},
                                                         "responseData": {"echo": d["d"]["requestData"]}}}).to_string())).unwrap();
            }
        });
        let events = Arc::new(Mutex::new(vec![]));
        let ev2 = events.clone();
        let ws = ObsWs::connect(port, "pw", move |t, _| ev2.lock().unwrap().push(t.to_string()), || {}).unwrap();
        let r = ws.request("StartRecord", Some(json!({"a": 1})), Duration::from_secs(3)).unwrap();
        assert_eq!(r, json!({"echo": {"a": 1}}));
        assert!(ws.request("Fail", None, Duration::from_secs(3)).unwrap_err().contains("nope"));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(events.lock().unwrap().as_slice(), ["RecordStateChanged"]);
    }
}
