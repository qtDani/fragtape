<img src="assets/icon.svg" width="64" height="64" alt="">

# Fragtape

Automatic game recording and highlights for Windows, in the style of Outplayed and Medal.
Fragtape notices when you launch a game and records it, or just its highlights. It
detects kills and other big moments, and keeps everything in a video library.

Fragtape is a small Rust app (a single `.exe` with a native window). For recording it uses a private,
hidden copy of the OBS Studio you already have installed. That copy only runs while
you play, and your own OBS setup is never touched.

## Using it

Start **Fragtape** from the Start menu. It lives in the tray:

- **Left-click** the tray icon to open the window.
- **Right-click** it for the menu: save a highlight, stop recording, quit.
- The icon turns **red** while recording and **amber** while saving highlights.

**New games and notices** appear as a small card in the top-right corner of the game's
window. It follows the window if you move it. If the window is minimized or too small,
the card goes to the corner of the monitor instead. Clicking the card doesn't take focus away from the game.

- **New game:** a card with *Record* / *Highlights* / *Nothing*. It hides itself after
  30 seconds; hovering over it pauses the countdown. Until the game closes, you can still
  answer in the window or the tray menu. If you don't answer, nothing is recorded and
  you're asked again next time.
- **Notices:** "Clip saved" and "Highlight marked" cards disappear after 3.5 seconds.
- **Exclusive fullscreen:** the card shows over borderless and windowed games. Games in
  exclusive fullscreen mode can hide it, like most overlays.

The window has three pages:

- **Tapes:** every recording and clip, grouped by game, with thumbnails. Filter by type
  (Highlights, Sessions, ★ favorites) or by game, or search.
  - **Right-click** a tape for a quick menu: Open, Rename (typed right on the card),
    Add to favorites, Show in folder, Delete.
  - Open one to play it. Kills and hotkey marks show as dots on the timeline and in
    the *Moments* list; click one to jump there.
  - Pick a start `[` and an end `]` to save that part as a new clip.
  - Clips can be renamed, starred and deleted. Deleting moves them to the Recycle Bin.
- **Games:** every game you've played. Each one has a mode:
  - **Ask:** ask me next time.
  - **Record:** the whole session.
  - **Clips:** only highlights, from the replay buffer.
  - **Off:** ignore the game.
  - You can search about 10,000 games or add an `.exe`. *Scan PC* imports your Steam
    history and installed Riot and Epic games.
- **Settings:** split into tabs. **General** (start with Windows, hotkey, notifications),
  **Recording**, **Advanced** (below), **Appearance** (the theme: *Sharp minimal*, the
  default, *Graphite*, *Blocks*, *Paper* or *Vista Aero*), **Storage**,
  **Kill detection** and **About** (version and activity log). The last tab you opened is remembered.

Videos are saved in `Videos\Fragtape\<Game>\`. Kills are also written as chapters inside
the file, so they show up in other video players too.

### Recording settings

| Setting | Options |
| --- | --- |
| Capture | **Game window**: Game Capture, with Window Capture underneath for games that can't be hooked. **Whole screen**: for anti-cheat games that come out black. |
| Audio | **Game** only · Game + mic · All PC sound |
| Record cursor | **On** · Off: the mouse pointer appears in the video when the game shows it |
| Microphone in mono | On (default) · Off |
| Quality | Small · Balanced · **High** · Max (constant quality on your GPU) · **Custom** (uses the advanced encoder settings) |
| Frame rate | 30 · **60** |
| Clip length | 15 s · **30 s** · 60 s · 2 min: how much the replay buffer keeps in Clips mode |
| Highlight hotkey | **F8** · F9 · F10 · Ctrl+F8 · Off |
| Start with Windows | On · **Off** (starts in the tray) |

### Advanced recording

The **video encoder** settings are used when Quality is set to *Custom*:

| Setting | Options |
| --- | --- |
| Codec | H.264 · HEVC · AV1. Codecs your GPU doesn't have are greyed out after the first recording. |
| Encoder | GPU (NVENC / AMD AMF / Intel QuickSync) · CPU (x264) |
| Rate control | Constant quality (CQ/CRF 1–51) · Variable bitrate (bitrate + max bitrate) · Constant bitrate (1,000–200,000 kbps, with an estimated size per hour) |
| Encoder preset | 1 (fastest) to 7 (best quality). NVENC p1–p7, AMF speed/balanced/quality, QuickSync TU7–TU1, x264 ultrafast–slow. |
| Keyframe interval | 1–10 s |
| B-frames | 0–4 |
| Look-ahead | On · Off (NVIDIA) |
| Color depth | 8-bit · 10-bit (HEVC/AV1) |

The **output, audio and performance** settings always apply:

| Setting | Options |
| --- | --- |
| Resolution | Native · 2160p · 1440p · 1080p · 900p · 720p (never upscales) |
| Scale filter | Lanczos · Bicubic · Bilinear · Area |
| Frame rate | Basic setting, or 24 / 30 / 48 / 60 / 90 / 120 / 144 / 165 / 240 fps |
| File format | **Hybrid MP4** (survives crashes, has chapters) · Hybrid MOV · MKV · MP4 |
| Audio bitrate | 96–320 kbps |
| Sample rate | 44.1 · 48 kHz |
| Audio tracks | One mix · Separate (1 everything, 2 game, 3 microphone, 4 desktop) |
| Replay buffer memory | 512 MB – 8 GB |
| OBS priority | Normal · **Above normal** · High |

Changes apply from the next game. If a game is being recorded right now, the change
applies once it closes.

## Kill detection

| Game | How | Detects |
| --- | --- | --- |
| League of Legends | Riot's Live Client Data API, built into the game | kills, deaths, assists, double to penta kills, aces, first blood, objectives and steals, turrets/inhibitors |
| Counter-Strike 2 | Valve Game State Integration (*Settings → Kill detection → Set up*) | kills, headshots, 3k/4k/ace rounds, deaths, MVPs |
| Dota 2 | the same, plus the launch option `-gamestateintegration` | kills, deaths, assists |
| Every other game | the highlight hotkey | whatever you mark |

Events within 10 s of each other are grouped into one *moment*. A moment becomes a clip
when its score reaches 2, which is about two kills. In Record mode, highlights are also
exported as separate clips into a `Highlights` folder if ffmpeg is installed. The app
can install ffmpeg for you.

## How it stays light

| | Between games | While recording |
| --- | --- | --- |
| Fragtape (tray) | ~25 MB RAM, ~0.13% of one CPU core | same |
| Hidden OBS | **not running** | ~150–300 MB; video encoded on the GPU |
| Window | only while open (native window) | |

- **Starting and stopping:** the hidden OBS starts in about 1.5 s when a game you
  record is detected, and closes 15 s after the game ends.
- **Where it runs:** OBS runs in portable mode from `%LOCALAPPDATA%\Fragtape\engine`.
  That folder holds directory links to your OBS install, so nothing is copied. It has
  its own settings, no preview and no third-party plugins.
- **Control:** Fragtape controls OBS through obs-websocket, with a random password that
  changes every start. obs-websocket can't be limited to this PC, so while a game is
  recorded it also listens on your network; without the password it can't be used. If
  Windows asks whether OBS may use networks, you can say no.
- **Internet use:** Fragtape downloads Discord's public game list once a week. Game
  icons come from the PC: the game's `.exe`, or Steam's local library cache. Only games
  that aren't installed get Discord's icon, downloaded once and then kept, so the window
  itself never loads anything from the internet.
- **The local API:** it listens only on `127.0.0.1:3133`, checks the Host header,
  and requires a secret per-install token.

## Files

- `%APPDATA%\Fragtape\`: `library.json` (games, modes, settings), `gamedb.json`
  (Discord's game list, refreshed weekly), `app.log`.
- `%LOCALAPPDATA%\Fragtape\`: `clips.json` (titles, markers, favorites), `thumbs\`, `icons\`,
  `engine\` (the hidden OBS's config).
- `%LOCALAPPDATA%\Programs\Fragtape\Fragtape.exe`: the app.

On first start, Fragtape takes over the games, settings and videos of OBSPlayed, its
earlier Python version.

**Requirements:** Windows 10/11 and OBS Studio 30 or newer.

## Building

```
cd fragtape
cargo test --release
cargo build --release      # target\release\fragtape.exe
```

| File | What it does |
| --- | --- |
| `app.rs` | wiring: shared state, main loop, window, first-run migration |
| `controller.rs` | game detection, modes, starting and stopping recordings, highlight moments |
| `engine.rs`, `obsws.rs` | the hidden OBS (layout, config, start/stop) and an obs-websocket client |
| `quality.rs` | presets and advanced settings mapped to OBS encoder settings |
| `clips.rs` | clip library, thumbnails, trim, Recycle Bin, post-recording work |
| `ui/*.slint`, `src/ui/` | the native window (Slint): Tapes, Games and Settings pages, the player |
| `video.rs` | video playback and thumbnails with Windows' Media Foundation |
| `server.rs` | the local API used by the window and the tray |
| `library.rs`, `gamedb.rs`, `importers.rs` | your games and settings, Discord's game list, Steam/Riot/Epic import |
| `providers.rs` | League, CS2 and Dota 2 kill detection, GSI setup |
| `tray.rs`, `win.rs`, `http.rs` | tray icon and hotkey, Win32 helpers, a WinHTTP client |

`legacy-python/` holds the earlier Python version: the OBS plugin and the first
standalone app.
