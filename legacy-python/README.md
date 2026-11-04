> **Legacy.** This is the earlier Python version (OBSPlayed). It has been replaced by
> [Fragtape](../README.md), the Rust app in `../fragtape`.

# OBSPlayed

Outplayed/Medal-style automatic game recording. OBSPlayed notices when you launch
a game, records it (or just its highlights), detects kills and other big moments,
and keeps everything in a clip library.

It comes in two forms:

- **The app** (recommended): a tray app with its own window. It records with a
  private, hidden copy of OBS Studio that only runs while you play. Your own OBS
  is never touched.
- **The OBS plugin** (the original version, described further down): a script
  inside your own OBS, managed from a dock.

## The app

Start **OBSPlayed** from the Start menu (or `pythonw OBSPlayed.pyw`). It lives in the
tray. Left-click the icon to open the window; right-click it for the menu (save a
highlight, stop recording, quit).

The window has three pages:

- **Clips:** every recording and clip, grouped by game, with thumbnails. Open one to
  play it. Kills and hotkey marks show as dots on the timeline and in the *Moments*
  list (click to jump there). Pick a start `[` and an end `]` to save part of it as a
  new clip. Clips can be renamed, starred and deleted (deleted clips go to the
  Recycle Bin).
- **Games:** the same game list, modes and Steam/Riot/Epic scan as the plugin's panel.
- **Settings:** Capture (game window / whole screen), Audio (game / game + mic / all),
  microphone in mono, Quality, Frame rate (30/60), Clip length (how much the replay
  buffer keeps), the highlight hotkey (default **F8**, works in game) and *Start with
  Windows*.

Videos are saved to `Videos\OBSPlayed\<Game>\`. Recordings are Hybrid MP4, and kills are
also written as chapters inside the file, so they show up in other video players too.

### How it stays light

| | Between games | While recording |
| --- | --- | --- |
| OBSPlayed (tray) | ~45 MB, ~0.2 % of one CPU core (a process list every 2 s) | same |
| Hidden OBS | **not running** | ~150-300 MB, video encoded on the GPU (NVENC/AMF/QuickSync) |
| Window | only while open (Edge in app mode) | |

The hidden OBS starts in about 1.5 s when a game you record is detected, and closes
15 s after the game ends. It runs from `%LOCALAPPDATA%\OBSPlayed\engine` in portable
mode, with directory links to your OBS install (nothing is copied), its own settings,
no preview, no third-party plugins, and no browser engine running. OBSPlayed controls it
through obs-websocket with a random password that changes every start. obs-websocket can't
be limited to this PC, so it also listens on your network while a game is recorded; without
the password it can't be used. If Windows asks whether OBS may use networks, you can say no:
OBSPlayed only connects from this PC.

Capture uses Game Capture with a Window Capture underneath. Window Capture fills in
when a game can't be hooked (some borderless or anti-cheat games). If a game still
comes out black, set Capture to *Whole screen*.

### App files

- `%APPDATA%\OBSPlayed\`: `library.json` (games, modes, settings), `gamedb.json`,
  `app.log`. On first start, the plugin's games and settings are copied over.
- `%LOCALAPPDATA%\OBSPlayed\`: `clips.json` (titles, markers, favorites), `thumbs\`,
  `engine\` (the hidden OBS's config), `window\` (the window's browser profile).

Requirements: Windows, OBS Studio 30+ installed (your own OBS setup isn't changed), Python 3.12
(`winget install Python.Python.3.12`). Optional: ffmpeg for saving clips from recordings
(the app can install it: *Settings → Clip editor*).

## The OBS plugin

### The OBSPlayed panel

- **Status:** what's running right now ("Counter-Strike 2, Recording whole session, 12:31").
  When a new game starts, the panel asks you what to record. A popup asks the same
  question, so you can answer from inside the game too.
- **New games:** what to do the first time a game is seen: *Ask*, *Record*, *Clips* or *Off*.
- **Add a game:** search ~10,000 games, or type an `.exe` name for anything else.
- **Your games:** every game you've played, with its icon, playtime and last-played date.
  Each one has its own mode. *Scan PC* imports your Steam history (every game played on
  your account, with playtime), plus installed Riot and Epic games. A scan also runs when
  OBS starts, but only if Steam/Riot/Epic files changed since the last one (a few file
  checks, otherwise skipped). A full scan takes about 20 ms and runs at idle priority.
  Game detection itself costs about 2 ms every 2 seconds.

| Mode | What happens |
| --- | --- |
| **Ask** | Ask me the next time I play it |
| **Record** (whole session) | Records the whole game. Each event becomes a **chapter marker** inside the video. When the game closes, recording stops and **highlight clips** are exported (`OBSPlayed Highlights/<recording>/01 - Triple Kill.mp4`) if ffmpeg is installed. |
| **Clips** (only highlights) | Doesn't record the session. Runs OBS's replay buffer and saves a clip when a moment gets hot or you press the hotkey (`Replay 2026-11-03 21-00-00 - Triple Kill.mov`). |
| **Off** | Ignores the game |

If you close the popup without choosing, nothing is recorded and you'll be asked next time.

### Recording settings (in the panel)

| Setting | Options |
| --- | --- |
| **Capture** | **Game only** (default): OBSPlayed keeps its own scene, *OBSPlayed*, with a Game Capture locked to the game's .exe and an Application Audio Capture of just that game. It switches to that scene when the game starts recording and back to your scene afterwards. Nothing else on your screen is recorded. **My scene**: record your current OBS scene as it is. |
| **Audio** | **Game** (default): only the game's sound, no Discord, music or notifications. **Game + mic**. **All PC sound**: everything plus your mic. OBS's global Desktop Audio and Mic/Aux go into every recording, so OBSPlayed mutes or unmutes them for the game and restores them afterwards. |
| **Quality** | **OBS** (keep your settings) · Small · Balanced · **High** (default) · Max. Recording gets its own encoder (your GPU's, e.g. NVENC H.264) at constant quality instead of a fixed bitrate. If recording used to share the streaming encoder, OBS has to be restarted once; the panel says so. After that, quality changes apply to the next recording. |
| **Frame rate** | OBS (keep) · 30 · 60. This is OBS's global frame rate, so it needs an OBS restart. |

OBSPlayed never changes your scene or audio while you're streaming, and it doesn't switch
scenes when you started the recording yourself.

**Adding the panel:** *Docks → Custom Browser Docks*. Name it `OBSPlayed` and paste the URL
from *Tools → Scripts → obsplayed.py → Copy panel URL*. The URL contains a secret key, so
other websites can't change your settings. The panel server only listens on `127.0.0.1:3132`.

"Clips" mode needs *Settings → Output → Replay Buffer → Enable Replay Buffer*, with a
length of at least 30 s.

Chapters need OBS's *Hybrid MP4* or *Hybrid MOV* recording format. With any other format,
OBSPlayed writes a highlights viewer (`<recording>.highlights.html`) and a JSON event list
(`<recording>.obsplayed.json`) next to the recording instead.

## Which games are detected

1. Built-in games (`obsplayed_core/games.py`). LoL, CS2 and Dota 2 also have kill detection.
2. Games in your library: added in the panel, imported from Steam/Riot/Epic, or played before.
3. **Discord's public game list** (`discord.com/api/v9/applications/detectable`). This is the
   list Discord uses to show "Playing …", with ~10,000 Windows games and their exe names.
   It's downloaded once, cached in `%APPDATA%\obs-studio\plugin_config\obsplayed\gamedb.json`
   and refreshed weekly. Exe names that many games share (`game.exe`, `launcher.exe`, …) only
   match when the game's folder matches too.

Medal and Outplayed don't publish their game lists, and Outplayed's kill events come from
Overwolf's private SDK. Games without kill detection rely on the **Mark highlight** hotkey.

## Highlight detection

| Game | How | Detects |
| --- | --- | --- |
| League of Legends | Riot Live Client Data API (built into the game) | kills, deaths, assists, double to penta kills, aces, first blood, dragon/baron/herald/grubs (and steals), turrets/inhibitors |
| Counter-Strike 2 | Valve Game State Integration | kills, headshots, 3k/4k/ace rounds, deaths, MVPs |
| Dota 2 | Valve Game State Integration | kills, deaths, assists (multi-kills are found by grouping kills that happen close together) |
| Every other game | **Mark highlight hotkey** | whatever you mark |

Nearby events (default: within 10 s) are grouped into one *moment*. Each event type has a
weight (kill 1, headshot 1.2, triple 2, penta 5, steal 4, manual 10, ...). A moment becomes
a clip when its total reaches the **sensitivity** value (default 2, about two kills close
together). Lower the value to get more clips.

## Install (plugin)

1. **Python for OBS.** Install 64-bit Python 3.12 (`winget install Python.Python.3.12`;
   OBS 32/33 fail to load 3.14 with "Could not load library"), then in OBS go to
   *Tools → Scripts → Python Settings* and pick the install folder.
2. Copy `obsplayed.py` **and** the `obsplayed_core/` folder to the same place.
3. In *Tools → Scripts*, click **+** and choose `obsplayed.py`.
4. Add the panel (see above).
5. Optional: *Settings → Hotkeys → "OBSPlayed: Mark highlight"*.
6. Optional: install ffmpeg (`winget install Gyan.FFmpeg`) to export clips in Record mode.
7. Recommended: *Settings → Output → Recording Format → Hybrid MP4* (or Hybrid MOV).

You don't need to set up a scene: with **Capture: Game only**, OBSPlayed builds its own.

### CS2 / Dota 2

Click **Install CS2 / Dota 2 GSI config** in the script settings, then restart the game.
Dota 2 also needs the launch option `-gamestateintegration`. The GSI server only listens
on `127.0.0.1:3131` and checks an auth token.

### League of Legends

Nothing to set up. Recording starts when the match loads (`League of Legends.exe`),
not in the client lobby.

## Script settings (Tools → Scripts)

Games and modes live in the panel. The script settings hold the rest:

| Setting | Default |
| --- | --- |
| Panel port | 3132 |
| Stop recording when the game closes / wait before stopping | on / 10 s |
| Switch to scene on game start (only with Capture: My scene) | don't switch |
| Detection: LoL, CS2/Dota 2 GSI (port, token) | on, 3131 |
| Add chapter markers | on |
| Clips: wait after the last event / save at most this long after a moment starts | 5 s / 15 s |
| Record mode: also save a replay clip on each highlight | off |
| Sensitivity / merge window / seconds before / seconds after | 2 / 10 s / 8 s / 4 s |
| Also write viewer + JSON / export clips / ffmpeg path | off / on / auto-detect |

OBSPlayed only stops recordings and replay buffers it started itself. The hotkey always
works: it adds a chapter if a recording is running, or saves a replay clip if only the
replay buffer is running.

## Development

```
py -m unittest discover -s tests -v
```

`obsplayed.py` (plugin) and `engine.py` (app) are the only files that talk to OBS.
Everything else in `obsplayed_core/` is plain Python and is tested without OBS:

- `app.py`, `app.html`: the standalone app (tray, window, wiring)
- `engine.py`, `obsws.py`: the hidden OBS (setup, config, start/stop) and a minimal obs-websocket client
- `clips.py`: the clip library (index, metadata, thumbnails, trim, delete)
- `tray.py`: tray icon and global hotkey (Win32)
- `controller.py`: game detection loop, modes, recording control, session lifecycle
- `library.py`: your games, modes and play history (`plugin_config/obsplayed/library.json`)
- `gamedb.py`: Discord's game list (download, cache, exe matching, search)
- `importers.py`: Steam / Riot / Epic import
- `quality.py`: recording quality presets → encoder settings
- `panel.py`, `panel.html`: the dock (local HTTP API + page)
- `modes.py`, `ask_dialog.ps1`: the first-time popup
- `providers/lol.py`, `providers/gsi.py`: kill detection (background threads, results go to the main thread through a queue)
- `session.py`: markers and highlight grouping/scoring
- `clipper.py`, `viewer.py`: output after a recorded session
- `games.py`: built-in games

Created in [T3 Code](https://t3.codes).
