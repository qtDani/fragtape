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
mod util;
mod win;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    win::make_dpi_aware();
    // A panic in a background thread would otherwise vanish silently (no console).
    std::panic::set_hook(Box::new(|info| util::log(format!("PANIC: {info}"))));
    if !win::single_instance(r"Local\FragtapeApp") {
        // Already running: ask that copy to show its window.
        if let Some(token) = util::read_json(&util::data_dir().join("library.json")).and_then(|v| v["token"].as_str().map(String::from)) {
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", app::PORT)) {
                use std::io::Write;
                let _ = write!(s, "POST /api/show HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Fragtape-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}", app::PORT);
                let _ = std::io::Read::read(&mut s, &mut [0u8; 64]);
            }
        }
        return;
    }
    app::run(!args.iter().any(|a| a == "--tray"));
}
