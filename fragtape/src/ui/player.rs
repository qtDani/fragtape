//! The native video player: Windows' video engine draws frames at the size they are shown,
//! and Slint draws the controls over them.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use slint::{ComponentHandle, Image, Model, Rgba8Pixel, SharedPixelBuffer, SharedString};

use super::data::{clock, model, moment_color};
use super::{AppWindow, Detail, Palette, Pin, PlayerState, Span, Stat};
use crate::video::{self, Video};

const SPEEDS: [f64; 5] = [0.25, 0.5, 1.0, 1.5, 2.0];
/// Moments are jumped to a little early, so you see what led up to them.
const LEAD_IN: f64 = 3.0;

/// One marker of the recording.
struct Mark {
    t: f64,
    kind: String,
    label: String,
    detail: String,
}

/// The summary tile a moment counts toward.
fn category(kind: &str) -> &'static str {
    match kind {
        "kill" | "headshot" | "first_blood" | "double_kill" | "triple_kill" | "quad_kill" | "penta_kill" => "kills",
        "death" => "deaths",
        "assist" => "assists",
        _ => "other",
    }
}

pub struct Player {
    pub id: String,
    video: Video,
    preview: Option<Video>,
    marks: Vec<Mark>,      // sorted by time
    now_mark: Option<usize>,
    duration: f64,
    primed: bool,          // played to the first frame once (needed before paused seeks show anything)
    autoplay: bool,
    buf: Vec<u8>,
    pub sel: Option<(f64, f64)>,
    /// Playing just the trim selection: where to stop.
    previewing: Option<f64>,
    dragging: Option<bool>, // Some(was playing)
    rate: usize,
    last_move: Instant,
    hover_want: Option<f64>,
    hover_busy: bool,
    hover_t: Instant,
    last_size: (u32, u32),
    redraw: bool,          // fetch a frame even if the engine says nothing is new (after a resize)
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

impl Player {
    pub fn open(win: &AppWindow, clip: &Value, path: PathBuf, volume: f64, muted: bool) -> Option<Player> {
        let video = Video::new(false).map_err(|e| crate::logf!("[ui] video engine: {e}")).ok()?;
        video.set_volume(volume);
        video.set_muted(muted);
        video.open(&path).ok()?;
        let st = win.global::<PlayerState>();
        let accent = win.global::<Palette>().get_accent();
        let dur = clip["duration"].as_f64().unwrap_or(0.0);
        let markers: Vec<&Value> = clip["markers"].as_array().map(|m| m.iter().collect()).unwrap_or_default();
        st.set_title(ss(clip["title"].as_str().unwrap_or("")));
        st.set_sub(ss(format!("{} · {}", clip["game"].as_str().unwrap_or(""), super::data::size(clip["size"].as_f64().unwrap_or(0.0)))));
        st.set_frame(Image::default());
        st.set_loading(true);
        st.set_playing(false);
        st.set_pos(0.0);
        st.set_buffered(0.0);
        st.set_volume(volume as f32);
        st.set_muted(muted);
        st.set_rate_text(ss("1×"));
        st.set_sel_on(false);
        st.set_hover_on(false);
        st.set_favorite(clip["favorite"] == true);
        st.set_note(ss(""));
        st.set_need_ffmpeg(false);
        st.set_idle(false);
        let kind = |m: &Value| m["kind"].as_str().map(String::from).unwrap_or_else(|| {
            if m["label"].as_str().is_some_and(|l| l.to_lowercase().contains("manual") || l.to_lowercase().contains("highlight")) { "manual".into() } else { "kill".into() }
        });
        let mut marks: Vec<Mark> = markers.iter().map(|m| Mark {
            t: m["offset"].as_f64().unwrap_or(0.0), kind: kind(m),
            label: m["label"].as_str().unwrap_or("").to_string(), detail: m["detail"].as_str().unwrap_or("").to_string(),
        }).collect();
        marks.sort_by(|a, b| a.t.total_cmp(&b.t));
        // summary tiles: a click jumps to the next moment of that kind
        let count = |cat: &str| marks.iter().filter(|m| category(&m.kind) == cat).count() as i32;
        let (k, d, a, o) = (count("kills"), count("deaths"), count("assists"), count("other"));
        let tile = |cat: &str, kind: &str, label: &str, n: i32| Stat { cat: ss(cat), kind: ss(kind), label: ss(label), count: n, color: moment_color(kind, accent) };
        st.set_stats(model(if marks.is_empty() { vec![] } else {
            vec![tile("kills", "kill", "Kills", k), tile("deaths", "death", "Deaths", d),
                 tile("assists", "assist", "Assists", a), tile("other", "objective", "Objectives & marks", o)]
        }));
        st.set_kda(ss(if k + d + a == 0 { String::new() } else {
            let ratio = (k + a) as f64 / d.max(1) as f64;
            format!("{k} / {d} / {a}  ·  KDA {}", if d == 0 { format!("{ratio:.0} (no deaths)") } else { format!("{ratio:.1}") })
        }));
        st.set_empty_moments(ss(if clip["kind"] == "session" {
            "No moments were detected in this recording. Press your highlight hotkey while playing to mark them." } else { "No kills or marks inside this clip." }));
        st.set_moment_count(marks.len() as i32);
        let mut details = vec![
            ("Game", clip["game"].as_str().unwrap_or("").to_string()),
            ("Type", if clip["kind"] == "session" { "Whole session".into() } else { "Clip".into() }),
            ("Recorded", super::data::ago(clip["mtime"].as_f64().unwrap_or(0.0))),
            ("Length", if dur > 0.0 { clock(dur) } else { "…".into() }),
            ("Size", super::data::size(clip["size"].as_f64().unwrap_or(0.0))),
            ("Video", "…".into()),
            ("File", clip["name"].as_str().unwrap_or("").to_string()),
        ];
        if let Some(h) = clip["highlights"].as_array().filter(|h| !h.is_empty()) {
            details.insert(4, ("Highlights", format!("{} (cut into clips)", h.len())));
        }
        st.set_details(model(details.into_iter().map(|(k, v)| Detail { k: ss(k), v: ss(v) }).collect()));
        let p = Player {
            id: clip["id"].as_str().unwrap_or("").to_string(), video, preview: None,
            marks, now_mark: Some(usize::MAX), duration: dur, primed: false, autoplay: true,
            buf: Vec::new(), sel: None, previewing: None, dragging: None, rate: 2, last_move: Instant::now(), hover_want: None, hover_busy: false,
            hover_t: Instant::now(), last_size: (0, 0), redraw: true,
        };
        p.draw_timeline(win, clip);
        Some(p)
    }

    fn dur(&self) -> f64 {
        let d = self.video.duration();
        if d > 0.0 { d } else { self.duration.max(0.001) }
    }

    /// Pins and highlight spans along the bar (needs the duration).
    pub fn draw_timeline(&self, win: &AppWindow, clip: &Value) {
        let st = win.global::<PlayerState>();
        let accent = win.global::<Palette>().get_accent();
        let d = self.dur();
        st.set_pins(model(clip["markers"].as_array().into_iter().flatten().map(|m| {
            let k = m["kind"].as_str().unwrap_or(if m["label"].as_str().is_some_and(|l| l.contains("Manual")) { "manual" } else { "kill" }).to_string();
            let off = m["offset"].as_f64().unwrap_or(0.0);
            Pin { x: (off / d).clamp(0.0, 1.0) as f32, color: moment_color(&k, accent), kind: ss(&k),
                  label: ss(format!("{}  {}", clock(off), m["label"].as_str().unwrap_or(""))), offset: off as f32 }
        }).collect()));
        st.set_spans(model(clip["highlights"].as_array().into_iter().flatten().map(|h| Span {
            a: (h["start"].as_f64().unwrap_or(0.0) / d).clamp(0.0, 1.0) as f32, b: (h["end"].as_f64().unwrap_or(0.0) / d).clamp(0.0, 1.0) as f32,
        }).collect()));
    }

    /// Called about 60 times a second while the player is open.
    pub fn tick(&mut self, win: &AppWindow, clip: &Value) {
        let st = win.global::<PlayerState>();
        let fl = self.video.flags.take();
        if fl & video::EV_LOADED != 0 && !self.primed {
            self.duration = self.video.duration();
            self.draw_timeline(win, clip);
            let (vw, vh) = self.video.native_size();
            let ds = st.get_details();
            for i in 0..ds.row_count() {
                let mut row = ds.row_data(i).unwrap();
                match row.k.as_str() {
                    "Length" => row.v = ss(clock(self.duration)),
                    "Video" => row.v = ss(if vw > 0 { format!("{vw}×{vh}") } else { "unknown".into() }),
                    _ => continue,
                }
                ds.set_row_data(i, row);
            }
            self.video.play(); // to the first frame; paused right away unless we autoplay
            self.primed = true;
            st.set_loading(false);
        }
        if fl & video::EV_ERROR != 0 {
            st.set_loading(false);
            st.set_note(ss("This video can't be played (the file may be broken, or use a format Windows can't decode)."));
        }
        if !self.autoplay && self.primed && !self.video.paused() && self.dragging.is_none() {
            // keep a paused state that was asked for before loading finished
        }
        // frames, at the size they are shown
        let scale = win.window().scale_factor();
        let (w, h) = ((st.get_video_w() * scale) as u32, (st.get_video_h() * scale) as u32);
        if w >= 16 && h >= 16 {
            if (w, h) != self.last_size { self.last_size = (w, h); self.redraw = true; }
            if self.video.new_frame() || self.redraw {
                if self.video.frame(w, h, &mut self.buf).is_ok() {
                    let px = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&self.buf, w, h);
                    st.set_frame(Image::from_rgba8(px));
                    self.redraw = false;
                }
            }
        }
        let d = self.dur();
        let t = self.video.time();
        // previewing the trim selection: stop at its end
        if let Some(end) = self.previewing {
            if t >= end - 0.05 || self.video.ended() {
                self.video.pause();
                self.seek_to(end, false);
                self.previewing = None;
            } else if self.video.paused() {
                self.previewing = None; // paused by hand
            }
        }
        st.set_previewing(self.previewing.is_some());
        st.set_playing(!self.video.paused() && !self.video.ended());
        if self.dragging.is_none() { st.set_pos((t / d).clamp(0.0, 1.0) as f32); }
        st.set_time_text(ss(format!("{} / {}", clock(t), clock(d))));
        self.show_now(win, t);
        st.set_buffered(1.0);
        // hide the controls while watching
        st.set_idle(!self.video.paused() && self.last_move.elapsed() > Duration::from_millis(2500) && self.dragging.is_none());
        // hover preview
        self.tick_preview(win);
    }

    fn tick_preview(&mut self, win: &AppWindow) {
        let st = win.global::<PlayerState>();
        let Some(pv) = self.preview.as_mut() else { return };
        let fl = pv.flags.take();
        if self.hover_busy && (fl & video::EV_SEEKED != 0 || self.hover_t.elapsed() > Duration::from_millis(400)) {
            if pv.new_frame() || fl & video::EV_SEEKED != 0 {
                let r = pv.frame(192, 108, &mut self.buf);
                if r.is_ok() {
                    st.set_hover_img(Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&self.buf, 192, 108)));
                }
                self.hover_busy = false;
            } else if self.hover_t.elapsed() > Duration::from_millis(800) {
                self.hover_busy = false;
            }
        }
        if !self.hover_busy {
            if let Some(t) = self.hover_want.take() {
                pv.seek(t);
                self.hover_busy = true;
                self.hover_t = Instant::now();
            }
        }
    }

    pub fn poke(&mut self) {
        self.last_move = Instant::now();
    }

    pub fn toggle(&mut self) {
        if self.video.paused() || self.video.ended() { self.video.play() } else { self.video.pause() }
        self.autoplay = !self.video.paused();
        self.poke();
    }

    pub fn seek_to(&mut self, t: f64, play: bool) {
        self.video.seek(t.clamp(0.0, self.dur()));
        self.redraw = true;
        if play { self.video.play(); }
    }

    pub fn press(&mut self, frac: f64) {
        self.dragging = Some(!self.video.paused());
        self.video.pause();
        self.seek_to(frac * self.dur(), false);
    }
    pub fn drag(&mut self, win: &AppWindow, frac: f64) {
        win.global::<PlayerState>().set_pos(frac as f32);
        self.seek_to(frac * self.dur(), false);
    }
    pub fn release(&mut self) {
        if let Some(was) = self.dragging.take() {
            if was { self.video.play(); }
        }
    }

    pub fn hover(&mut self, win: &AppWindow, frac: f64, path: &std::path::Path) {
        let st = win.global::<PlayerState>();
        let d = self.dur();
        let t = frac * d;
        if self.preview.is_none() {
            if let Ok(v) = Video::new(true) {
                if v.open(path).is_ok() { self.preview = Some(v); }
            }
        }
        let near = self.marks.iter().find(|m| ((m.t - t) / d).abs() < 0.012).map(|m| m.label.clone());
        st.set_hover_text(ss(match near { Some(l) => format!("{} · {l}", clock(t)), None => clock(t) }));
        st.set_hover_x(frac as f32);
        st.set_hover_on(true);
        self.hover_want = Some(t);
        // the preview engine needs one play before paused seeks show frames
        if let Some(pv) = &self.preview {
            if pv.ready() && pv.time() == 0.0 && !self.hover_busy { pv.play(); pv.pause(); }
        }
        self.poke();
    }

    pub fn set_volume(&mut self, v: f64) {
        self.video.set_volume(v);
        self.video.set_muted(v <= 0.0);
    }
    pub fn set_muted(&mut self, m: bool) { self.video.set_muted(m); }

    pub fn speed(&mut self) -> String {
        self.rate = (self.rate + 1) % SPEEDS.len();
        self.video.set_rate(SPEEDS[self.rate]);
        format!("{}×", SPEEDS[self.rate])
    }

    pub fn now(&self) -> f64 { self.video.time() }
    pub fn duration(&self) -> f64 { self.dur() }
    pub fn pause(&mut self) { self.video.pause(); }
    pub fn paused(&self) -> bool { self.video.paused() }

    pub fn jump_moment(&mut self, dir: i32) {
        let t = self.video.time();
        let next = if dir > 0 { self.marks.iter().find(|m| m.t - LEAD_IN > t + 0.5) } else { self.marks.iter().rev().find(|m| m.t - LEAD_IN < t - 1.0) };
        if let Some(o) = next.map(|m| m.t) { self.seek_to(o - LEAD_IN, true); }
    }

    /// The next moment of a summary tile's kind, back to the first one after the last.
    pub fn jump_cat(&mut self, cat: &str) {
        let t = self.video.time();
        let of: Vec<f64> = self.marks.iter().filter(|m| category(&m.kind) == cat).map(|m| m.t).collect();
        if let Some(o) = of.iter().find(|o| **o - LEAD_IN > t + 0.5).or(of.first()).copied() {
            self.seek_to(o - LEAD_IN, true);
        }
    }

    /// The "now" box: the last moment at or before the playhead.
    fn show_now(&mut self, win: &AppWindow, t: f64) {
        let cur = self.marks.iter().rposition(|m| m.t <= t + 0.25);
        if cur == self.now_mark { return; }
        self.now_mark = cur;
        let st = win.global::<PlayerState>();
        let accent = win.global::<Palette>().get_accent();
        match cur.map(|i| &self.marks[i]) {
            Some(m) => {
                st.set_now_kind(ss(&m.kind));
                st.set_now_color(moment_color(&m.kind, accent));
                st.set_now_label(ss(format!("{}  {}", clock(m.t), m.label)));
                st.set_now_detail(ss(&m.detail));
            }
            None => {
                st.set_now_kind(ss(""));
                st.set_now_label(ss(if self.marks.is_empty() { "No moments in this video".to_string() }
                                    else { format!("First moment at {}", clock(self.marks[0].t)) }));
                st.set_now_detail(ss(if self.marks.is_empty() { "" } else { "Press Next to jump there" }));
            }
        }
    }

    /// Show the trim box and its text.
    pub fn draw_sel(&self, win: &AppWindow, ffmpeg: bool) {
        let st = win.global::<PlayerState>();
        match self.sel {
            Some((a, b)) => {
                let d = self.dur();
                st.set_sel_on(true);
                st.set_sel_a((a / d) as f32);
                st.set_sel_b((b / d) as f32);
                st.set_sel_text(ss(format!("{}–{} ({} s)", clock(a.round()), clock(b.round()), (b.round() - a.round()) as i64)));
                st.set_can_save(ffmpeg && b - a >= 1.0);
                st.set_need_ffmpeg(!ffmpeg);
                st.set_note(ss(if ffmpeg { "" } else { "Saving clips needs ffmpeg." }));
            }
            None => {
                st.set_sel_on(false);
                st.set_need_ffmpeg(false);
                st.set_note(ss(""));
            }
        }
    }
    pub fn trim_start(&mut self) {
        let (t, d) = (self.video.time(), self.dur());
        self.sel = Some(((t - 15.0).max(0.0), (t + 15.0).min(d)));
    }
    /// Drag a trim handle: the video shows the frame under it, so you see where the clip starts/ends.
    pub fn trim_move(&mut self, handle: i32, frac: f64) {
        let t = frac * self.dur();
        let Some((a, b)) = self.sel.as_mut() else { return };
        if handle == 0 { *a = t.min(*b - 1.0).max(0.0) } else { *b = t.max(*a + 1.0) }
        let at = if handle == 0 { *a } else { *b };
        self.previewing = None;
        self.video.pause();
        self.seek_to(at, false);
    }
    /// Play only the selection, from its start, and stop at its end. Pressing it again stops.
    pub fn preview_sel(&mut self) {
        if self.previewing.take().is_some() {
            self.video.pause();
            return;
        }
        if let Some((a, b)) = self.sel {
            self.seek_to(a, true);
            self.previewing = Some(b);
        }
    }
    pub fn stop_preview(&mut self) {
        if self.previewing.take().is_some() { self.video.pause(); }
    }
    pub fn mark_in(&mut self) {
        let (t, d) = (self.video.time(), self.dur());
        let b = self.sel.map(|s| s.1).filter(|b| *b > t).unwrap_or((t + 15.0).min(d));
        self.sel = Some((t, b));
    }
    pub fn mark_out(&mut self) {
        let t = self.video.time();
        let a = self.sel.map(|s| s.0).filter(|a| *a < t).unwrap_or((t - 15.0).max(0.0));
        self.sel = Some((a, t));
    }
    pub fn step(&mut self, frames: f64) {
        self.video.pause();
        self.seek_to(self.video.time() + frames / 60.0, false);
    }
    pub fn nudge(&mut self, secs: f64) {
        let play = !self.video.paused();
        self.seek_to(self.video.time() + secs, play);
    }
}
