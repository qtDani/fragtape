"""OBSPlayed as a standalone app: tray icon + clip library window + hidden OBS.

    pythonw OBSPlayed.pyw          start (opens the window)
    pythonw OBSPlayed.pyw --tray   start in the tray only (used at Windows startup)

Idle cost: one process list every 2 s (~2 ms). OBS itself only runs while a
game is being recorded, and the window (Edge in app mode) only while it's open.
"""
from __future__ import annotations

import ctypes
import json
import mimetypes
import os
import shutil
import subprocess
import sys
import threading
import time
import webbrowser
from collections import deque
from ctypes import wintypes
from pathlib import Path
from urllib.request import Request, urlopen

from . import __version__, clipper, modes
from .clips import ClipStore, videos_root
from .controller import Controller, Settings
from .engine import Engine, EngineObs, engine_options, pick_encoder
from .gamedb import GameDB
from .library import Library
from .library import default_dir as plugin_dir
from .panel import Panel
from .procwatch import make_dpi_aware
from .tray import Tray

PORT = 3133
NO_WINDOW = 0x08000000
IDLE_STOP_AFTER = 15.0  # seconds without a game/recording before the hidden OBS is closed


def data_dir() -> Path:
    return Path(os.environ.get("APPDATA") or Path.home()) / "OBSPlayed"


def local_dir() -> Path:
    return Path(os.environ.get("LOCALAPPDATA") or Path.home()) / "OBSPlayed"


class AppLibrary(Library):
    PREF_CHOICES = {
        "capture": ("game", "screen"),           # the game window, or the whole monitor (anti-cheat fallback)
        "audio": ("game", "game_mic", "all"),
        "quality": ("small", "balanced", "high", "max"),
        "fps": (30, 60),
        "mic_mono": (True, False),
        "clip_length": (15, 30, 60, 120),        # seconds kept by the replay buffer (Clips mode)
        "hotkey": ("F8", "F9", "F10", "Ctrl+F8", "off"),
        "autostart": (True, False),
    }
    PREF_DEFAULTS = {"capture": "game", "audio": "game", "quality": "high", "fps": 60, "mic_mono": True,
                     "clip_length": 30, "hotkey": "F8", "autostart": False}


# ---- Windows helpers ---------------------------------------------------------------
def _find_app_window() -> int | None:
    """The open OBSPlayed window (Edge app window titled "OBSPlayed")."""
    u = ctypes.WinDLL("user32")
    found: list = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        title = ctypes.create_unicode_buffer(64)
        cls = ctypes.create_unicode_buffer(64)
        u.GetWindowTextW(hwnd, title, 64)
        u.GetClassNameW(hwnd, cls, 64)
        if title.value == "OBSPlayed" and cls.value.startswith("Chrome_WidgetWin") and u.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    u.EnumWindows(visit, 0)
    return found[0] if found else None


def _edge_path() -> str | None:
    try:
        import winreg
        for root in (winreg.HKEY_CURRENT_USER, winreg.HKEY_LOCAL_MACHINE):
            try:
                with winreg.OpenKey(root, r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe") as k:
                    path = winreg.QueryValueEx(k, "")[0]
                    if Path(path).exists():
                        return path
            except OSError:
                pass
    except ImportError:
        pass
    for base in (os.environ.get("ProgramFiles(x86)"), os.environ.get("ProgramFiles")):
        if base and (Path(base) / "Microsoft/Edge/Application/msedge.exe").exists():
            return str(Path(base) / "Microsoft/Edge/Application/msedge.exe")
    return None


RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"


def set_autostart(enabled: bool) -> None:
    import winreg
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as k:
        if enabled:
            exe = Path(sys.executable)
            pythonw = exe.with_name("pythonw.exe") if exe.with_name("pythonw.exe").exists() else exe
            script = Path(__file__).resolve().parent.parent / "OBSPlayed.pyw"
            winreg.SetValueEx(k, "OBSPlayed", 0, winreg.REG_SZ, f'"{pythonw}" "{script}" --tray')
        else:
            try:
                winreg.DeleteValue(k, "OBSPlayed")
            except OSError:
                pass


# ---- the web UI server ------------------------------------------------------------------
class AppServer(Panel):
    def __init__(self, app: "App") -> None:
        super().__init__(app.library, app.gamedb, lambda: app.controller, port=PORT, log=app.log)
        self.app = app
        self.page = Path(__file__).with_name("app.html")

    def state(self) -> dict:
        s = super().state()
        a = self.app
        s["app"] = {
            "version": __version__,
            "obs_found": a.engine.available,
            "engine": a.engine.running,
            "recording": a.api.recording,
            "replay": a.api.replay,
            "ffmpeg": bool(a.ffmpeg()),
            "installing_ffmpeg": a.installing_ffmpeg,
            "folder": str(videos_root()),
            "hotkey": a.library.prefs()["hotkey"],
            "log": list(a.logs)[-40:],
        }
        return s

    def post(self, path: str, body: dict) -> dict:
        out = super().post(path, body)
        if path == "/api/pref":
            self.app.pref_changed(str(body["key"]))
        return out

    def post_extra(self, path: str, body: dict) -> dict:
        a, clips = self.app, self.app.clips
        if path == "/api/clip/update":
            clips.update(str(body["id"]), **{k: v for k, v in body.items() if k != "id"})
        elif path == "/api/clip/delete":
            clips.delete(str(body["id"]))
        elif path == "/api/clip/trim":
            return {"ok": True, **clips.trim(str(body["id"]), float(body["start"]), float(body["end"]),
                                             a.ffmpeg(), str(body.get("title") or ""))}
        elif path == "/api/clip/reveal":
            p = clips.path_for(str(body["id"]))
            if p:
                subprocess.Popen(["explorer", "/select,", str(p)])
        elif path == "/api/open-folder":
            videos_root().mkdir(parents=True, exist_ok=True)
            os.startfile(videos_root())
        elif path == "/api/mark":
            a.mark()
        elif path == "/api/stop":
            a.api.stop_recording() if a.api.recording else a.api.stop_replay()
        elif path == "/api/install-ffmpeg":
            a.install_ffmpeg()
        elif path == "/api/show":
            a.open_window()
        else:
            raise KeyError(path)
        return {"ok": True}

    def post_raw(self, path: str, query: dict, data: bytes) -> dict:
        if path == "/api/thumb":
            self.app.clips.save_thumb(query.get("id", [""])[0], data)
            return {"ok": True}
        raise KeyError(path)

    def get_extra(self, h, path: str, query: dict) -> bool:
        if path == "/api/clips":
            h._json({"clips": self.app.clips.list(live={self.app.api.active_path} if self.app.api.active_path else set())})
            return True
        if path.startswith("/thumb/"):
            p = self.app.clips.thumbs / f"{path[7:]}.jpg"
            if not p.exists() or "/" in path[7:] or "\\" in path[7:]:
                h._json({"error": "not found"}, 404)
                return True
            data = p.read_bytes()
            h.send_response(200)
            h.send_header("Content-Type", "image/jpeg")
            h.send_header("Content-Length", str(len(data)))
            h.send_header("Cache-Control", "private, max-age=86400")
            h.end_headers()
            h.wfile.write(data)
            return True
        if path.startswith("/media/"):
            p = self.app.clips.path_for(path[7:])
            if not p:
                h._json({"error": "not found"}, 404)
                return True
            _send_file(h, p)
            return True
        return False


def _send_file(h, path: Path) -> None:
    """Serve a video with HTTP range support, so the player can seek."""
    size = path.stat().st_size
    ctype = "video/mp4" if path.suffix.lower() in (".mp4", ".mov") else (mimetypes.guess_type(path.name)[0] or "video/x-matroska")
    start, end = 0, size - 1
    rng = h.headers.get("Range", "")
    if rng.startswith("bytes="):
        a, _, b = rng[6:].split(",")[0].partition("-")
        if a:
            start, end = int(a), int(b) if b else size - 1
        elif b:
            start = max(0, size - int(b))
        end = min(end, size - 1)
        h.send_response(206)
        h.send_header("Content-Range", f"bytes {start}-{end}/{size}")
    else:
        h.send_response(200)
    h.send_header("Content-Type", ctype)
    h.send_header("Accept-Ranges", "bytes")
    h.send_header("Content-Length", str(end - start + 1))
    h.end_headers()
    with open(path, "rb") as f:
        f.seek(start)
        left = end - start + 1
        while left > 0:
            chunk = f.read(min(512 * 1024, left))
            if not chunk:
                break
            h.wfile.write(chunk)
            left -= len(chunk)


# ---- the app ---------------------------------------------------------------------------------
class App:
    def __init__(self) -> None:
        self.data = data_dir()
        self.local = local_dir()
        self.data.mkdir(parents=True, exist_ok=True)
        self.local.mkdir(parents=True, exist_ok=True)
        self.logs: deque[str] = deque(maxlen=300)
        self._logfile = open(self.data / "app.log", "a", encoding="utf-8", buffering=1)
        self.stopping = False
        self.installing_ffmpeg = False
        self._ffmpeg: str | None = None
        self._ffmpeg_checked = 0.0
        self._stopping_engine = False
        self._migrate()
        self.library = AppLibrary(self.data / "library.json")
        self.gamedb = GameDB(self.data, self.log)
        self.clips = ClipStore(videos_root(), self.local, self.log)
        self.engine = Engine(self.log, engine_dir=self.local / "engine")
        self.encoder = pick_encoder(self.engine.obs_dir)
        self.api = EngineObs(self.engine, self.options, self.log)
        self.controller: Controller | None = None
        self.server = AppServer(self)
        self.tray = Tray(self.local, on_open=self.open_window, on_quit=self.quit, on_hotkey=self.mark,
                         menu_extra=self._menu, log=self.log)

    # ---- logging / setup --------------------------------------------------------------
    def log(self, msg: str) -> None:
        line = f"{time.strftime('%H:%M:%S')} {msg}"
        self.logs.append(line)
        try:
            self._logfile.write(line + "\n")
        except (OSError, ValueError):
            pass

    def _migrate(self) -> None:
        """First start: take over the games and settings of the OBS plugin version."""
        old = plugin_dir()
        for name in ("library.json", "gamedb.json"):
            if not (self.data / name).exists() and (old / name).exists():
                shutil.copy2(old / name, self.data / name)

    def options(self) -> dict:
        c = self.controller
        game = c.current.name if c and c.current else "Other"
        return engine_options(self.library.prefs(), videos_root() / clipper.safe_name(game), self.engine.obs_dir,
                              pids=c.current_pids if c else None, encoder=self.encoder)

    def ffmpeg(self) -> str | None:
        if not self._ffmpeg and time.monotonic() - self._ffmpeg_checked > 10:
            self._ffmpeg_checked = time.monotonic()
            self._ffmpeg = clipper.find_ffmpeg("")
        return self._ffmpeg

    def install_ffmpeg(self) -> None:
        if self.installing_ffmpeg:
            return
        self.installing_ffmpeg = True

        def run() -> None:
            self.log("installing ffmpeg (winget install Gyan.FFmpeg)…")
            try:
                r = subprocess.run(["winget", "install", "--id", "Gyan.FFmpeg", "-e", "--silent",
                                    "--accept-package-agreements", "--accept-source-agreements"],
                                   capture_output=True, text=True, creationflags=NO_WINDOW, timeout=900)
                self.log("ffmpeg installed" if r.returncode == 0 else f"ffmpeg install failed: {r.stdout[-200:]}")
            except Exception as exc:
                self.log(f"ffmpeg install failed: {exc!r}")
            finally:
                self._ffmpeg_checked = 0.0
                self.installing_ffmpeg = False

        threading.Thread(target=run, daemon=True).start()

    # ---- actions ---------------------------------------------------------------------------
    def open_window(self) -> None:
        hwnd = _find_app_window()
        if hwnd:
            u = ctypes.windll.user32
            u.ShowWindow(hwnd, 9)  # SW_RESTORE
            u.SetForegroundWindow(hwnd)
            return
        url = self.server.url
        edge = _edge_path()
        if edge:
            subprocess.Popen([edge, f"--app={url}", f"--user-data-dir={self.local / 'window'}",
                              "--window-size=1280,820", "--no-first-run", "--no-default-browser-check",
                              "--disable-features=Translate"], creationflags=NO_WINDOW)
        else:
            webbrowser.open(url)

    def mark(self) -> None:
        if self.controller:
            self.controller.mark_manual()

    def pref_changed(self, key: str) -> None:
        p = self.library.prefs()
        if key == "hotkey":
            self.tray.set_hotkey(p["hotkey"])
        elif key == "autostart":
            try:
                set_autostart(p["autostart"])
            except OSError as exc:
                self.log(f"could not change autostart: {exc}")
        elif key in ("mic_mono", "fps", "capture", "clip_length") and self.engine.running:
            self.log(f"{key}: applies from the next game")

    def _menu(self) -> list:
        c = self.controller
        st = c.status if c else {}
        items: list = []
        if st.get("game"):
            what = "Recording" if self.api.recording else "Clips on" if self.api.replay else "Playing"
            items.append((f"{what}: {st['game']}", None))
            items.append((f"Save highlight ({self.library.prefs()['hotkey']})", self.mark))
        if self.api.recording:
            items.append(("Stop recording", self.api.stop_recording))
        items.append(("Open videos folder", lambda: os.startfile(videos_root()) if videos_root().exists() else None))
        return items

    def quit(self) -> None:
        self.stopping = True
        self.tray.quit()

    def _on_media(self, info: dict) -> None:
        self.clips.add(info)
        if info.get("kind") == "clip":
            self.tray.notify("Clip saved", f"{info.get('game') or ''}: {info.get('title') or 'Highlight'}".strip(": "))

    # ---- main loop ---------------------------------------------------------------------------
    def _dispatch(self, what: str | None) -> None:
        c = self.controller
        if what == "recording_started":
            c.on_recording_started()
        elif what == "recording_stopped":
            c.on_recording_stopped()
            c.on_output_stopped()
        elif what == "replay_saved":
            c.on_replay_saved()
        elif what == "replay_stopped":
            c.on_output_stopped()

    def _idle_check(self) -> None:
        c = self.controller
        busy = self.api.recording or self.api.replay or (c.current is not None and c.mode in (modes.FULL, modes.HIGHLIGHTS))
        if (self.engine.running and not busy and not self._stopping_engine
                and time.monotonic() - self.api.idle_since > IDLE_STOP_AFTER):
            self._stopping_engine = True

            def stop() -> None:
                try:
                    self.engine.stop()
                finally:
                    self._stopping_engine = False

            threading.Thread(target=stop, name="engine-stop", daemon=True).start()

    def _update_tray(self) -> None:
        st = self.controller.status
        game = st.get("game")
        if self.api.recording:
            self.tray.set_state("rec", f"OBSPlayed: recording {game or ''}".strip())
        elif self.api.replay:
            self.tray.set_state("clip", f"OBSPlayed: saving highlights of {game or 'your game'}")
        elif game:
            self.tray.set_state("watching", f"OBSPlayed: {game} (not recording)")
        else:
            self.tray.set_state("watching", "OBSPlayed: watching for games")

    def loop(self) -> None:
        c = self.controller
        while not self.stopping:
            try:
                while True:
                    try:
                        kind, data = self.engine.events.get_nowait()
                    except Exception:
                        break
                    self._dispatch(self.api.handle_event(kind, data))
                c.tick()
                self._idle_check()
                self._update_tray()
            except Exception as exc:
                self.log(f"loop error: {exc!r}")
            time.sleep(0.5)

    def shutdown(self) -> None:
        if self.api.recording:
            self.log("quitting: stopping the recording first")
            self.api.stop_recording()
            deadline = time.monotonic() + 15
            while self.api.recording and time.monotonic() < deadline:
                try:
                    kind, data = self.engine.events.get(timeout=0.5)
                    self._dispatch(self.api.handle_event(kind, data))
                except Exception:
                    pass
        if self.api.replay:
            self.api.stop_replay()
        self.engine.stop()
        if self.controller:
            self.controller.shutdown()
        self.server.stop()
        self.log("bye")

    def run(self, show_window: bool) -> None:
        self.log(f"OBSPlayed {__version__} starting (OBS: {self.engine.obs_dir or 'not found'}, encoder {self.encoder})")
        settings = Settings(write_session_files=False, cut_clips=True)
        self.controller = Controller(self.api, settings, log=self.log, library=self.library, gamedb=self.gamedb)
        self.controller.on_media = self._on_media
        self.api.before_start = self.controller.on_output_starting
        self.server.start()
        self.gamedb.start(on_ready=lambda: self.server.scan_async())
        self.tray.set_hotkey(self.library.prefs()["hotkey"])
        threading.Thread(target=self.loop, name="obsplayed-loop", daemon=True).start()
        if show_window:
            threading.Timer(0.3, self.open_window).start()
        try:
            self.tray.run()  # blocks until Quit
        finally:
            self.stopping = True
            self.shutdown()


def _already_running() -> bool:
    k = ctypes.WinDLL("kernel32", use_last_error=True)
    k.CreateMutexW.restype = wintypes.HANDLE
    k.CreateMutexW.argtypes = [wintypes.LPVOID, wintypes.BOOL, wintypes.LPCWSTR]
    _already_running.handle = k.CreateMutexW(None, False, r"Local\OBSPlayedApp")  # held for the process lifetime
    return ctypes.get_last_error() == 183  # ERROR_ALREADY_EXISTS


def main(argv: list[str] | None = None) -> None:
    argv = sys.argv[1:] if argv is None else argv
    make_dpi_aware()
    if _already_running():
        # Ask the running copy to show its window.
        try:
            token = json.loads((data_dir() / "library.json").read_text(encoding="utf-8"))["token"]
            urlopen(Request(f"http://127.0.0.1:{PORT}/api/show", data=b"{}", method="POST",
                            headers={"X-OBSPlayed-Token": token, "Content-Type": "application/json"}), timeout=3)
        except Exception:
            pass
        return
    App().run(show_window="--tray" not in argv)
