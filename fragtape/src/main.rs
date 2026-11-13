//! Fragtape: automatic game recording and highlights, powered by a hidden OBS.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod clips;
mod controller;
mod engine;
mod events;
mod gamedb;
mod http;
mod icons;
mod importers;
mod library;
mod logo;
mod obsws;
mod providers;
mod quality;
mod server;
mod toast;
mod tray;
mod ui;
mod util;
mod video;
mod win;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    win::make_dpi_aware();
    // A panic in a background thread would otherwise vanish silently (no console).
    std::panic::set_hook(Box::new(|info| util::log(format!("PANIC: {info}"))));
    const MUTEX: &str = r"Local\FragtapeApp";
    // `--quit` (used by the installer): ask a running copy to finish its recording and close,
    // and wait up to 60 s until it's gone.
    if args.iter().any(|a| a == "--quit") {
        if win::instance_running(MUTEX) {
            send("/api/quit");
            for _ in 0..240 {
                if !win::instance_running(MUTEX) { break; }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        return;
    }
    if !win::single_instance(MUTEX) {
        // Already running: ask that copy to show its window.
        send("/api/show");
        return;
    }
    app::run(!args.iter().any(|a| a == "--tray"));
}

/// POST an empty request to the running copy's local API.
fn send(path: &str) {
    let Some(token) = util::read_json(&util::data_dir().join("library.json")).and_then(|v| v["token"].as_str().map(String::from)) else { return };
    if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", app::PORT)) {
        use std::io::Write;
        let _ = write!(s, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Fragtape-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}", app::PORT);
        let _ = std::io::Read::read(&mut s, &mut [0u8; 64]);
    }
}
