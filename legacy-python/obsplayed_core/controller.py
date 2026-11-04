"""Core logic: detect games, drive recording, collect highlight markers.

Everything OBS-specific goes through the small ``ObsApi`` interface so this
module can be tested without OBS. ``tick()`` must be called periodically from
the OBS main thread; provider threads only ever push into the EventBus.

Each game has a mode (see ``modes.py``):
  full        record the whole session; events become chapters in the video
  highlights  run the replay buffer only; save a clip when a moment gets hot
  off         do nothing
New games either use the default mode or trigger a one-time prompt.
"""
from __future__ import annotations

import json
import queue
import threading
import time
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Callable, Protocol

from . import clipper, modes, viewer
from .events import EventBus, GameEvent
from .gamedb import GameDB
from .games import KNOWN_GAMES, Game, detect_game
from .library import Library
from .procwatch import main_window, obs_window_spec, running_processes
from .providers.gsi import GSIServer
from .providers.lol import LeagueProvider
from .session import Session, find_highlights, title_for


@dataclass
class Settings:
    stop_on_exit: bool = True
    exit_grace: float = 10.0
    game_scene: str = ""
    lol_enabled: bool = True
    gsi_enabled: bool = True
    gsi_port: int = 3131
    gsi_token: str = "obsplayed"
    add_chapters: bool = True
    save_replay_on_highlight: bool = False
    replay_delay: float = 5.0
    replay_max_wait: float = 15.0
    min_score: float = 2.0
    merge_gap: float = 10.0
    pre_roll: float = 8.0
    post_roll: float = 4.0
    write_session_files: bool = False  # viewer .html + .json; forced on if chapters fail
    cut_clips: bool = True
    ffmpeg_path: str = ""
    poll_interval: float = 2.0


class ObsApi(Protocol):
    def recording_active(self) -> bool: ...
    def start_recording(self) -> None: ...
    def stop_recording(self) -> None: ...
    def add_chapter(self, name: str) -> bool: ...
    def replay_active(self) -> bool: ...
    def start_replay(self) -> None: ...
    def stop_replay(self) -> None: ...
    def save_replay(self) -> None: ...
    def set_scene(self, name: str) -> bool: ...
    def last_recording(self) -> str | None: ...
    def last_replay(self) -> str | None: ...
    # Capture: switch to OBSPlayed's game scene (game capture + game audio) and back.
    def prepare_capture(self, window_spec: str, audio: str) -> bool: ...
    def update_capture(self, window_spec: str) -> None: ...
    def restore_capture(self) -> None: ...
    # Writes encoder / fps settings; returns info for the panel.
    def apply_quality(self, prefs: dict) -> dict: ...


# asker(game_name, callback) shows the prompt; callback(game_name, mode or None)
Asker = Callable[[str, Callable[[str, "str | None"], None]], None]


@dataclass
class _Moment:
    first: float
    last: float
    score: float = 0.0
    events: list[GameEvent] = field(default_factory=list)


class Controller:
    def __init__(
        self,
        api: ObsApi,
        settings: Settings,
        log: Callable[[str], None] = print,
        process_lister: Callable[[], set[str]] = running_processes,
        clock: Callable[[], float] = time.monotonic,
        start_providers: bool = True,
        asker: Asker | None = None,
        library: Library | None = None,
        gamedb: GameDB | None = None,
    ) -> None:
        self.api = api
        self.log = log
        self.list_processes = process_lister
        self.clock = clock
        self.asker = asker or (lambda game, cb: modes.ask_with_dialog(game, cb, self.log))
        self.library = library if library is not None else Library()
        self.gamedb = gamedb
        self.bus = EventBus()
        self.settings = settings
        self.current: Game | None = None
        self.current_info: dict = {}
        self.current_since = 0.0
        self.current_exe: str | None = None
        self.current_pids: list[int] = []
        self.capture_active = False
        self.capture_request: dict | None = None
        self.window_found = False
        self.quality_due = True  # apply recording quality on the first tick
        self.quality_info: dict = {}
        self._running = None
        self.status: dict = {"game": None, "mode": None, "asking": [], "recording": False, "replay": False}
        self.mode: str | None = None  # mode of the current game; None while waiting for an answer
        self.game_gone_since: float | None = None
        self.we_started_recording = False
        self.we_started_replay = False
        self.session: Session | None = None
        self.next_poll = 0.0
        self.moment: _Moment | None = None
        self.replay_due: float | None = None
        self.replay_titles: list[str] = []
        self.asking: set[str] = set()
        self.close_prompt: dict[str, Callable[[], None]] = {}
        self.answers: queue.Queue[tuple[str, str | None]] = queue.Queue()
        self.start_providers = start_providers
        self.lol: LeagueProvider | None = None
        self.gsi: GSIServer | None = None
        self.finalizers: list[threading.Thread] = []
        # Called (from a worker thread) with each finished video: the standalone app's clip library.
        self.on_media: Callable[[dict], None] | None = None
        self.apply_settings(settings)
        if api.recording_active():
            # Script (re)loaded mid-recording: offsets are relative to now.
            self.session = Session(started_mono=self.clock())

    # ---- configuration -------------------------------------------------
    def apply_settings(self, s: Settings) -> None:
        old = self.settings
        self.settings = replace(s)
        if not self.start_providers:
            return
        gsi_changed = (old.gsi_port, old.gsi_token, old.gsi_enabled) != (s.gsi_port, s.gsi_token, s.gsi_enabled)
        if self.gsi and (gsi_changed or not s.gsi_enabled):
            self.gsi.stop()
            self.gsi = None
        if s.gsi_enabled and not self.gsi:
            self.gsi = GSIServer(self.bus, s.gsi_port, s.gsi_token, self.log)
            self.gsi.start()
        if not s.lol_enabled and self.lol:
            self._stop_lol()
        elif self.current and self.current.provider == "lol" and self.mode in (modes.FULL, modes.HIGHLIGHTS):
            self._start_lol()

    def mode_for(self, game: str) -> str:
        return self.library.mode_for(game)

    def answer(self, game: str, mode: str | None) -> None:
        """Answer the first-time question (thread-safe; used by the panel)."""
        self.answers.put((game, mode))

    # ---- detection -------------------------------------------------------
    def detect(self, running) -> tuple[Game | None, dict]:
        game = detect_game(running, KNOWN_GAMES)
        if game:
            info: dict = {"source": "builtin", "exes": list(game.exes)}
            db = self.gamedb.for_exe(game.exes[0]) if self.gamedb else None
            if db:
                info.update(discord_id=db.id, icon=db.icon)
            return game, info
        for exe, name in self.library.custom_exes():
            if _exe_running(exe, running):
                return Game(name, (exe,)), {}
        if self.gamedb:
            db = self.gamedb.match(running)
            if db:
                return Game(db.name, db.exes), {"source": "discord", "discord_id": db.id, "icon": db.icon, "exes": list(db.exes)}
        return None, {}

    def _update_status(self) -> None:
        self.status = {
            "game": self.current.name if self.current else None,
            "mode": self.mode,
            "since": self.current_since,
            "asking": sorted(self.asking),
            "recording": bool(self.api.recording_active()),
            "replay": bool(self.api.replay_active()),
            "capture": "game" if self.capture_active else "scene",
            "window_found": self.window_found,
            "quality": self.quality_info,
        }

    def prefs_changed(self) -> None:
        """Recording preferences changed in the panel (thread-safe)."""
        self.quality_due = True

    def _apply_quality(self) -> None:
        self.quality_due = False
        try:
            self.quality_info = self.api.apply_quality(self.library.prefs()) or {}
        except Exception as exc:
            self.quality_info = {"error": str(exc)}
            self.log(f"could not apply recording quality: {exc!r}")

    # ---- capture ---------------------------------------------------------
    # Scene switches must happen on OBS's UI thread (tick() runs on the graphics
    # thread, and switching scenes from there deadlocks OBS). So _start_capture
    # only records a request; OBS's "recording/replay starting" event, which
    # arrives on the UI thread right before the first frame, carries it out.
    def _start_capture(self) -> None:
        prefs = self.library.prefs()
        if prefs["capture"] == "scene" or not self.current_exe:  # "game" / "screen" (standalone app) capture the game
            if self.settings.game_scene:
                self.capture_request = {"scene": self.settings.game_scene}
            return
        window = main_window(self.current_pids)
        self.window_found = window is not None
        self.capture_request = {"spec": obs_window_spec(self.current_exe, window), "audio": prefs["audio"]}

    def on_output_starting(self) -> None:
        """OBS is about to start recording / the replay buffer (UI thread)."""
        req, self.capture_request = self.capture_request, None
        if not req:
            return
        if "scene" in req:
            if not self.api.set_scene(req["scene"]):
                self.log(f"scene '{req['scene']}' not found")
            return
        self.capture_active = bool(self.api.prepare_capture(req["spec"], req["audio"]))
        if self.capture_active:
            self.log(f"capturing {self.current_exe} (audio: {req['audio']})")

    def on_output_stopped(self) -> None:
        """Recording / replay buffer stopped (UI thread): put the user's scene back after a game."""
        if self.current is None and not (self.api.recording_active() or self.api.replay_active()):
            self._stop_capture()

    def _refresh_window(self) -> None:
        # Games often open their real window a few seconds after the process starts.
        if self.capture_active and not self.window_found and self.current_pids:
            window = main_window(self.current_pids)
            if window:
                self.window_found = True
                self.api.update_capture(obs_window_spec(self.current_exe, window))

    def _stop_capture(self) -> None:
        if self.capture_active:
            self.api.restore_capture()
        self.capture_active = False
        self.window_found = False

    def _start_lol(self) -> None:
        if self.start_providers and self.settings.lol_enabled and not self.lol:
            self.lol = LeagueProvider(self.bus, self.log)
            self.lol.start()

    def _stop_lol(self) -> None:
        if self.lol:
            self.lol.stop()
            self.lol = None

    # ---- main loop -----------------------------------------------------
    def tick(self) -> None:
        now = self.clock()
        while True:
            try:
                game, mode = self.answers.get_nowait()
            except queue.Empty:
                break
            self._on_answer(game, mode)
        if self.quality_due:
            self._apply_quality()
        if now >= self.next_poll:
            self.next_poll = now + self.settings.poll_interval
            self._poll_game(now)
        for event in self.bus.drain():
            self._handle_event(event)
        if self.replay_due is not None and now >= self.replay_due:
            self._flush_moment()
        self._update_status()

    def _poll_game(self, now: float) -> None:
        try:
            running = self.list_processes()
            game, info = self.detect(running)
        except Exception as exc:
            self.log(f"process scan failed: {exc!r}")
            return

        self._running = running
        if game and (self.current is None or game.name != self.current.name):
            if self.current:
                self._game_ended()
            self._game_started(game, info)
        elif game:
            self.game_gone_since = None
            self._refresh_window()
        elif self.current:
            if self.game_gone_since is None:
                self.game_gone_since = now
            elif now - self.game_gone_since >= self.settings.exit_grace:
                self._game_ended()

    def _game_started(self, game: Game, info: dict | None = None) -> None:
        self.current = game
        self.current_info = info or {}
        self.current_since = time.time()
        self.mode = None
        self.game_gone_since = None
        running = self._running if self._running is not None else set()
        self.current_exe = next((e.rsplit("/", 1)[-1] for e in game.exes if _exe_running(e, running)), None)
        self.current_pids = running.pids(self.current_exe) if self.current_exe and hasattr(running, "pids") else []
        self.library.game_started(game.name, **self.current_info)
        mode = self.mode_for(game.name)
        self.log(f"game detected: {game.name} (mode: {mode})")
        if mode == modes.ASK:
            if game.name not in self.asking:
                self.asking.add(game.name)
                self.log(f"asking what to record for {game.name}")
                cancel = self.asker(game.name, lambda g, m: self.answers.put((g, m)))
                if callable(cancel):
                    self.close_prompt[game.name] = cancel
            return
        self._activate(mode)

    def _on_answer(self, game: str, mode: str | None) -> None:
        self.asking.discard(game)
        close = self.close_prompt.pop(game, None)
        if close:
            close()  # answered from the panel: close the popup
        if mode is None:
            self.log(f"no choice made for {game}: not recording this time, will ask again next time")
            mode = modes.OFF
        else:
            self.library.set_mode(game, mode)
            self.log(f"saved mode for {game}: {mode}")
        if self.current and self.current.name == game and self.mode is None:
            self._activate(mode)

    def _activate(self, mode: str) -> None:
        game = self.current
        assert game is not None
        self.mode = mode
        if mode == modes.OFF:
            self.log(f"not recording {game.name}")
            return
        if game.provider == "lol":
            self._start_lol()
        if mode == modes.FULL:
            if not self.api.recording_active():
                self._start_capture()  # applied when OBS reports "recording starting"
                self.log("starting recording")
                self.api.start_recording()
                self.we_started_recording = True
            elif self.session and self.session.game is None:
                # You started this recording yourself: your scene stays as it is.
                self.session.game = game.name
        elif mode == modes.HIGHLIGHTS and not self.api.replay_active():
            self._start_capture()
            self.log("starting replay buffer (highlights only)")
            self.api.start_replay()
            self.we_started_replay = True

    def _game_ended(self) -> None:
        game = self.current
        if self.replay_due is not None:
            self._flush_moment()  # don't lose a clip that was waiting for its aftermath
        if game:
            self.library.game_ended(game.name, time.time() - self.current_since)
        self.current = None
        self.mode = None
        self.game_gone_since = None
        self._stop_lol()
        self.log(f"game closed: {game.name if game else '?'}")
        if self.we_started_recording and self.settings.stop_on_exit and self.api.recording_active():
            self.log("stopping recording")
            self.api.stop_recording()
        if self.we_started_replay and self.api.replay_active():
            self.log("stopping replay buffer")
            self.api.stop_replay()
        # The scene and audio are restored by on_output_stopped (UI thread).
        self.capture_request = None
        self.we_started_recording = False
        self.we_started_replay = False
        self.current_exe = None
        self.current_pids = []

    # ---- events ----------------------------------------------------------
    def _handle_event(self, event: GameEvent) -> None:
        manual = event.kind == "manual"
        if not manual and (self.current is None or event.game != self.current.name):
            return
        if self.session and (self.mode == modes.FULL or manual):
            marker = self.session.add(event)
            self.log(f"highlight event @ {marker.offset:.1f}s: {marker.label} {marker.detail}".rstrip())
            if not (self.settings.add_chapters and self.api.add_chapter(marker.label)):
                self.session.chapters_ok = False
            if self.settings.save_replay_on_highlight:
                self._add_to_moment(event)
        elif self.mode == modes.HIGHLIGHTS or manual:
            # Highlights-only mode, or the hotkey with no recording running.
            self._add_to_moment(event)

    def _add_to_moment(self, event: GameEvent) -> None:
        """Group events into a moment; save a replay once it gets hot enough."""
        now = self.clock()
        s = self.settings
        m = self.moment
        if m is None or now - m.last > s.merge_gap:
            m = self.moment = _Moment(first=now, last=now)
        m.last = now
        m.score += float(event.weight or 0.0)
        m.events.append(event)
        self.log(f"[highlights] {event.label} {event.detail}".rstrip() + f" (moment score {m.score:g})")
        if m.score >= s.min_score:
            # Wait a little so the clip includes the aftermath, but not so long
            # that the start of the moment falls out of the replay buffer.
            self.replay_due = min(now + s.replay_delay, m.first + s.replay_max_wait)

    def _flush_moment(self) -> None:
        events = self.moment.events if self.moment else []
        self.moment = None
        self.replay_due = None
        title = title_for([Session(0).add(e) for e in events]) if events else "Highlight"
        if not self.api.replay_active():
            self.log(f"[replay] '{title}' not saved: the replay buffer isn't running "
                     "(enable it in Settings > Output > Replay Buffer)")
            return
        self.replay_titles.append(title)
        self.api.save_replay()
        self.log(f"[replay] saving clip: {title}")

    def mark_manual(self) -> None:
        name = self.current.name if self.current else "Manual"
        self.bus.emit(GameEvent("manual", name, "hotkey"))

    # ---- OBS frontend events -------------------------------------------
    def on_recording_started(self) -> None:
        self.session = Session(started_mono=self.clock(), game=self.current.name if self.current else None)
        self.log("recording started, session open")

    def on_recording_stopped(self) -> None:
        session, self.session = self.session, None
        path = self.api.last_recording()
        if not session or not path:
            return
        settings = replace(self.settings)
        t = threading.Thread(target=self._finalize, args=(session, path, settings), name="obsplayed-finalize", daemon=True)
        self.finalizers = [f for f in self.finalizers if f.is_alive()] + [t]
        t.start()

    def on_replay_saved(self) -> None:
        path = self.api.last_replay()
        if not path or not self.replay_titles:
            return  # saved by the user from OBS itself: leave the name alone
        title = self.replay_titles.pop(0)
        game = self.current.name if self.current else None

        def rename() -> None:
            final = _rename_replay(path, title, self.log) or path
            if self.on_media:
                self.on_media({"kind": "clip", "path": final, "game": game, "title": title})

        threading.Thread(target=rename, name="obsplayed-rename", daemon=True).start()

    def _finalize(self, session: Session, path: str, s: Settings) -> None:
        recording = _wait_for_recording(path)
        if not recording:
            self.log(f"recording file not found: {path}")
            return
        highlights = find_highlights(
            session.markers, merge_gap=s.merge_gap, min_score=s.min_score, pre=s.pre_roll, post=s.post_roll
        )
        # Chapters inside the video already hold the timeline; only write the
        # viewer + JSON when asked to, or when the chapters didn't make it in.
        chapters_missing = bool(session.markers) and not session.chapters_ok
        if s.write_session_files or chapters_missing:
            if chapters_missing and not s.write_session_files:
                self.log("chapters unavailable for this recording format; writing viewer + JSON instead")
            data = session.to_dict(Path(recording).name, highlights)
            rec = Path(recording)
            try:
                rec.with_name(rec.stem + ".obsplayed.json").write_text(json.dumps(data, indent=2), encoding="utf-8")
                page = viewer.write_viewer(recording, data)
                self.log(f"session viewer: {page}")
            except OSError as exc:
                self.log(f"could not write session files: {exc}")
        self.log(f"session done: {len(session.markers)} events, {len(highlights)} highlights")
        if self.on_media:
            try:
                self.on_media({"kind": "session", "path": recording, "game": session.game,
                               **session.to_dict(Path(recording).name, highlights)})
            except Exception as exc:
                self.log(f"could not add the recording to the library: {exc!r}")
        if s.cut_clips and highlights:
            ffmpeg = clipper.find_ffmpeg(s.ffmpeg_path)
            if not ffmpeg:
                self.log("ffmpeg not found; skipping clip export (set its path in the script settings)")
                return
            clips = clipper.cut_clips(ffmpeg, recording, highlights, self.log)
            self.log(f"exported {len(clips)} clips to {clipper.clips_dir(recording)}")

    def shutdown(self) -> None:
        self._stop_capture()  # put the user's scene and audio back
        self._stop_lol()
        if self.gsi:
            self.gsi.stop()
            self.gsi = None


def _exe_running(exe: str, running) -> bool:
    """``exe`` may include parent folders ("win64/cs2.exe"); then the path must match too."""
    base = exe.rsplit("/", 1)[-1]
    if base not in running:
        return False
    if "/" not in exe:
        return True
    paths = running.paths(base) if hasattr(running, "paths") else []
    return any(p.endswith("/" + exe) for p in paths)


def _rename_replay(path: str, title: str, log=print) -> str | None:
    """Rename "Replay 2026-11-03 21-00-00.mkv" to "... - Triple Kill.mkv"."""
    src = Path(path)
    dst = src.with_name(f"{src.stem} - {clipper.safe_name(title)}{src.suffix}")
    for _ in range(20):
        try:
            src.rename(dst)
            log(f"[replay] saved {dst.name}")
            return str(dst)
        except FileNotFoundError:
            return None
        except OSError:
            time.sleep(0.5)  # OBS may still be writing it
    log(f"[replay] could not rename {src.name}")
    return None


def _wait_for_recording(path: str, timeout: float = 60.0) -> str | None:
    """Wait until OBS has finished writing (and possibly remuxing) the file."""
    p = Path(path)
    mp4 = p.with_suffix(".mp4")
    deadline = time.monotonic() + timeout
    last: tuple[str, int] | None = None
    stable = 0
    time.sleep(2.0)  # give OBS's auto-remux a chance to start
    while time.monotonic() < deadline:
        # Prefer the remuxed MP4 if OBS is producing one.
        target = mp4 if mp4.exists() else (p if p.exists() else None)
        if target:
            cur = (str(target), target.stat().st_size)
            stable = stable + 1 if cur == last and cur[1] > 0 else 0
            if stable >= 2:
                return str(target)
            last = cur
        time.sleep(1.0)
    return str(mp4 if mp4.exists() else p) if (mp4.exists() or p.exists()) else None
