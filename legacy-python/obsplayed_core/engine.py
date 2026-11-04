"""The recording engine: a private, hidden copy of OBS Studio.

OBSPlayed doesn't touch your own OBS. It starts the installed OBS in portable
mode from ``%LOCALAPPDATA%\\OBSPlayed\\engine`` (directory junctions to the
OBS install, so nothing is copied), with its own settings, minimized to the
tray and with the preview off. It only runs while a game is being recorded, and
it's controlled through obs-websocket on 127.0.0.1 with a random password.
"""
from __future__ import annotations

import json
import os
import queue
import secrets
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path
from typing import Callable

from . import quality
from .obsws import ObsWs, ObsWsError
from .procwatch import gpu_names, monitor_size

SCENE = "OBSPlayed"
GAME_SOURCE = "Game"
SCREEN_SOURCE = "Screen"
WINDOW_SOURCE = "Game Window"  # fallback under Game Capture: games that can't be hooked (borderless, anti-cheat)
AUDIO_SOURCE = "Game Audio"
DESKTOP_AUDIO = "Desktop Audio"
MIC = "Mic/Aux"
PROFILE = "OBSPlayed"
FORCE_MONO = 1 << 1  # OBS_SOURCE_FLAG_FORCE_MONO
NO_WINDOW = 0x08000000  # CREATE_NO_WINDOW


def find_obs() -> Path | None:
    """The OBS Studio install folder (the one containing bin\\64bit\\obs64.exe)."""
    candidates = []
    if sys.platform == "win32":
        try:
            import winreg
            for root in (winreg.HKEY_LOCAL_MACHINE, winreg.HKEY_CURRENT_USER):
                try:
                    with winreg.OpenKey(root, r"SOFTWARE\OBS Studio") as k:
                        candidates.append(winreg.QueryValueEx(k, "")[0])
                except OSError:
                    pass
        except ImportError:
            pass
    for env in ("ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"):
        if os.environ.get(env):
            candidates.append(os.path.join(os.environ[env], "obs-studio"))
    for c in candidates:
        if c and (Path(c) / "bin" / "64bit" / "obs64.exe").exists():
            return Path(c)
    return None


def default_engine_dir() -> Path:
    base = os.environ.get("LOCALAPPDATA") or str(Path.home())
    return Path(base) / "OBSPlayed" / "engine"


def pick_encoder(obs_dir: Path | None, gpus: list[str] | None = None) -> str:
    gpus = [g.lower() for g in (gpus if gpus is not None else gpu_names())]
    if any("nvidia" in g for g in gpus):
        new_nvenc = obs_dir is None or (obs_dir / "core" / "obs-nvenc").exists() \
            or (obs_dir / "obs-plugins" / "64bit" / "obs-nvenc.dll").exists()
        return "obs_nvenc_h264_tex" if new_nvenc else "jim_nvenc"
    if any("radeon" in g or "amd" in g for g in gpus):
        return "h264_texture_amf"
    if any("intel" in g for g in gpus):
        return "obs_qsv11_v2"
    return "obs_x264"


ENCODER_NAMES = {
    "obs_nvenc_h264_tex": "NVIDIA NVENC H.264", "jim_nvenc": "NVIDIA NVENC H.264",
    "h264_texture_amf": "AMD AMF H.264", "obs_qsv11_v2": "Intel QuickSync H.264", "obs_x264": "x264 (CPU)",
}


# ---- config files ---------------------------------------------------------------
def _ini(sections: dict[str, dict]) -> str:
    out = []
    for name, values in sections.items():
        out.append(f"[{name}]")
        out += [f"{k}={v}" for k, v in values.items()]
        out.append("")
    return "\n".join(out)


def _source(name: str, kind: str, settings: dict, muted: bool = False, flags: int = 0, mixers: int = 255) -> dict:
    return {
        "name": name, "uuid": str(uuid.uuid5(uuid.NAMESPACE_URL, "obsplayed/" + name)), "id": kind,
        "versioned_id": kind, "settings": settings, "mixers": mixers, "sync": 0, "flags": flags, "volume": 1.0,
        "balance": 0.5, "enabled": True, "muted": muted, "monitoring_type": 0, "private_settings": {},
    }


def _item(source: dict, item_id: int, w: int, h: int, visible: bool) -> dict:
    return {
        "name": source["name"], "source_uuid": source["uuid"], "visible": visible, "locked": True,
        "rot": 0.0, "pos": {"x": 0.0, "y": 0.0}, "scale": {"x": 1.0, "y": 1.0}, "align": 5,
        "bounds_type": 2, "bounds_align": 0, "bounds": {"x": float(w), "y": float(h)},  # 2 = scale to fit inside
        "crop_left": 0, "crop_top": 0, "crop_right": 0, "crop_bottom": 0, "id": item_id,
        "group_item_backup": False, "scale_filter": "disable", "blend_method": "default", "blend_type": "normal",
        "private_settings": {},
    }


def audio_mutes(audio: str) -> dict[str, bool]:
    """Which sources are muted for each audio choice."""
    return {
        DESKTOP_AUDIO: audio != "all",
        MIC: audio == "game",
        AUDIO_SOURCE: audio == "all",  # "all" already contains the game through Desktop Audio
    }


def scene_collection(capture: str, window_spec: str, audio: str, mic_mono: bool, size: tuple[int, int]) -> dict:
    w, h = size
    mutes = audio_mutes(audio)
    game = _source(GAME_SOURCE, "game_capture", {
        "capture_mode": "window", "window": window_spec, "priority": 2,  # 2 = match the .exe
        "capture_cursor": False, "allow_transparency": False, "anti_cheat_hook": True, "capture_audio": False,
    }, mixers=0)
    window = _source(WINDOW_SOURCE, "window_capture", {
        "window": window_spec, "priority": 2, "method": 2,  # 2 = Windows Graphics Capture
        "cursor": False, "client_area": True, "compatibility": False,
    }, mixers=0)
    screen = _source(SCREEN_SOURCE, "monitor_capture", {"monitor": 0, "capture_cursor": True}, mixers=0)
    game_audio = _source(AUDIO_SOURCE, "wasapi_process_output_capture", {"window": window_spec, "priority": 2},
                         muted=mutes[AUDIO_SOURCE])
    # First item = bottom layer. Game Capture covers the window capture once it hooks the game.
    scene = _source(SCENE, "scene", {"id_counter": 4, "custom_size": False, "items": [
        _item(window, 1, w, h, capture != "screen"),
        _item(game, 2, w, h, capture != "screen"),
        _item(screen, 3, w, h, capture == "screen"),
        _item(game_audio, 4, w, h, True),
    ]}, mixers=0)
    return {
        "name": PROFILE,
        "DesktopAudioDevice1": _source(DESKTOP_AUDIO, "wasapi_output_capture", {"device_id": "default"},
                                       muted=mutes[DESKTOP_AUDIO]),
        "AuxAudioDevice1": _source(MIC, "wasapi_input_capture", {"device_id": "default"},
                                   muted=mutes[MIC], flags=FORCE_MONO if mic_mono else 0),
        "sources": [scene, window, game, screen, game_audio],
        "groups": [], "scene_order": [{"name": SCENE}],
        "current_scene": SCENE, "current_program_scene": SCENE,
        "current_transition": "Cut", "transition_duration": 0, "transitions": [], "quick_transitions": [],
        "saved_projectors": [], "preview_locked": True, "scaling_enabled": False,
        "modules": {"scripts-tool": []}, "version": 2,
    }


def write_config(cfg: Path, o: dict) -> None:
    """Write the hidden OBS's whole configuration. ``o`` = engine options."""
    root = cfg / "obs-studio"
    prof = root / "basic" / "profiles" / PROFILE
    for d in (prof, root / "basic" / "scenes", root / "plugin_config" / "obs-websocket"):
        d.mkdir(parents=True, exist_ok=True)
    (root / "user.ini").write_text(_ini({
        "General": {"FirstRun": "true", "ConfirmOnExit": "false", "Pre19Defaults": "false", "Pre21Defaults": "false",
                    "Pre23Defaults": "false", "Pre24.1Defaults": "false", "EnableAutoUpdates": "false",
                    "WarnBeforeStartingStream": "false", "WarnBeforeStoppingRecord": "false"},
        "BasicWindow": {"PreviewEnabled": "false", "SysTrayEnabled": "true", "SysTrayWhenStarted": "true",
                        "ShowTransitions": "false"},
        "Basic": {"Profile": PROFILE, "ProfileDir": PROFILE, "SceneCollection": PROFILE,
                  "SceneCollectionFile": PROFILE + ".json", "ConfigOnNewProfile": "false"},
    }), encoding="utf-8")
    gl = root / "global.ini"
    if not gl.exists():
        gl.write_text("", encoding="utf-8")
    w, h = o["size"]
    rec_dir = str(o["rec_dir"]).replace("\\", "\\\\")
    fps = int(o.get("fps") or 60)
    (prof / "basic.ini").write_text(_ini({
        "General": {"Name": PROFILE},
        "Output": {"Mode": "Advanced", "FilenameFormatting": "%CCYY-%MM-%DD %hh-%mm-%ss"},
        "SimpleOutput": {"RecRBPrefix": "Clip", "FilePath": rec_dir, "RecFormat2": "hybrid_mp4"},
        "AdvOut": {
            "RecType": "Standard", "RecFilePath": rec_dir, "RecFormat2": "hybrid_mp4",
            "RecEncoder": o["encoder"], "RecAudioEncoder": "ffmpeg_aac", "RecTracks": 1, "TrackIndex": 1,
            "Track1Bitrate": 192, "RecUseRescale": "false", "Encoder": "obs_x264",
            "RecRB": "true", "RecRBTime": int(o.get("replay_seconds") or 30), "RecRBSize": 1024,
            "RecRBPrefix": "Clip",
        },
        "Video": {"BaseCX": w, "BaseCY": h, "OutputCX": w, "OutputCY": h, "FPSType": 0, "FPSCommon": fps,
                  "ScaleType": "bicubic", "ColorFormat": "NV12", "ColorSpace": "709", "ColorRange": "Partial"},
        "Audio": {"SampleRate": 48000, "ChannelSetup": "Stereo"},
    }), encoding="utf-8")
    (prof / "recordEncoder.json").write_text(
        json.dumps(quality.encoder_settings(o["encoder"], o.get("quality") or "high")), encoding="utf-8")
    coll = scene_collection(o.get("capture", "game"), o.get("window_spec", ""), o.get("audio", "game"),
                            bool(o.get("mic_mono")), (w, h))
    (root / "basic" / "scenes" / (PROFILE + ".json")).write_text(json.dumps(coll, indent=2), encoding="utf-8")
    (root / "plugin_config" / "obs-websocket" / "config.json").write_text(json.dumps({
        "alerts_enabled": False, "auth_required": True, "first_load": False, "server_enabled": True,
        "server_password": o["ws_password"], "server_port": o["ws_port"],
    }, indent=2), encoding="utf-8")


def _junction(link: Path, target: Path) -> None:
    if link.exists() or link.is_symlink():
        try:
            if os.path.realpath(link) == os.path.realpath(target):
                return
        except OSError:
            pass
        if not os.path.isjunction(link):
            # A real folder from an older layout: it only ever holds junctions.
            for child in link.iterdir():
                if os.path.isjunction(child):
                    os.rmdir(child)
        os.rmdir(link)  # an old junction (OBS moved): rmdir removes the link, not the target
    subprocess.run(["cmd", "/c", "mklink", "/J", str(link), str(target)], check=True,
                   capture_output=True, creationflags=NO_WINDOW)


def ensure_layout(obs_dir: Path, engine_dir: Path) -> Path:
    """Build the portable engine folder; returns the path of obs64.exe inside it."""
    engine_dir.mkdir(parents=True, exist_ok=True)
    for d in ("bin", "data", "core"):
        if (obs_dir / d).exists():
            _junction(engine_dir / d, obs_dir / d)
    if not (obs_dir / "core").exists():
        _junction(engine_dir / "obs-plugins", obs_dir / "obs-plugins")  # OBS before 33
    else:
        (engine_dir / "obs-plugins").mkdir(exist_ok=True)  # leave out third-party plugins
    (engine_dir / "config").mkdir(exist_ok=True)
    return engine_dir / "bin" / "64bit" / "obs64.exe"


def close_main_window(pid: int) -> bool:
    """Post WM_CLOSE to the "OBS <version> - ..." window of ``pid`` (it's hidden in the tray)."""
    if sys.platform != "win32":
        return False
    import ctypes
    from ctypes import wintypes
    u = ctypes.WinDLL("user32")
    found: list = []
    owner = wintypes.DWORD()

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        u.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid:
            title = ctypes.create_unicode_buffer(256)
            u.GetWindowTextW(hwnd, title, 256)
            if title.value.startswith("OBS "):
                found.append(hwnd)
        return True

    u.EnumWindows(visit, 0)
    for hwnd in found:
        u.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE
    return bool(found)


def _free_port(preferred: int) -> int:
    import socket
    for port in (preferred, 0):
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", port))
                return s.getsockname()[1]
            except OSError:
                continue
    return preferred


# ---- process + connection ---------------------------------------------------------
class Engine:
    def __init__(self, log: Callable[[str], None] = print, engine_dir: Path | None = None,
                 obs_dir: Path | None = None, ws_port: int = 4466) -> None:
        self.log = log
        self.engine_dir = engine_dir or default_engine_dir()
        self.obs_dir = obs_dir or find_obs()
        self.ws_port = ws_port
        self.password = secrets.token_urlsafe(18)
        self.proc: subprocess.Popen | None = None
        self.ws: ObsWs | None = None
        self.events: queue.Queue[tuple[str, dict]] = queue.Queue()
        self.started_at = 0.0
        self.options: dict = {}
        self._lock = threading.RLock()

    @property
    def available(self) -> bool:
        return self.obs_dir is not None

    @property
    def running(self) -> bool:
        return self.proc is not None and self.proc.poll() is None and self.ws is not None and not self.ws.closed

    def start(self, options: dict) -> None:
        with self._lock:
            if self.running:
                return
            if not self.obs_dir:
                raise RuntimeError("OBS Studio is not installed (https://obsproject.com)")
            self.stop()
            exe = ensure_layout(self.obs_dir, self.engine_dir)
            port = _free_port(self.ws_port)
            self.options = dict(options, ws_port=port, ws_password=self.password)
            write_config(self.engine_dir / "config", self.options)
            t0 = time.monotonic()
            self.proc = subprocess.Popen(
                [str(exe), "--portable", "--multi", "--minimize-to-tray", "--disable-updater",
                 "--disable-shutdown-check", "--disable-missing-files-check"],
                cwd=str(exe.parent), creationflags=NO_WINDOW)
            deadline = t0 + 40
            last: Exception | None = None
            while time.monotonic() < deadline:
                if self.proc.poll() is not None:
                    raise RuntimeError(f"OBS exited during startup (code {self.proc.returncode})")
                ws = ObsWs("127.0.0.1", port, self.password, on_event=self._on_event, on_close=self._on_close)
                try:
                    ws.connect()
                    self.ws = ws
                    break
                except (OSError, ObsWsError) as exc:
                    last = exc
                    time.sleep(0.4)
            # obs-websocket answers "not ready" until OBS has loaded its scenes.
            while self.ws and time.monotonic() < deadline:
                try:
                    self.ws.request("GetRecordStatus")
                    break
                except ObsWsError as exc:
                    last = exc
                    time.sleep(0.3)
            else:
                self.ws = None
            if not self.ws:
                self.stop(force=True)
                raise RuntimeError(f"could not connect to the recording engine: {last!r}")
            self.started_at = time.monotonic()
            self.log(f"[engine] OBS started in {self.started_at - t0:.1f}s (pid {self.proc.pid})")

    def stop(self, force: bool = False, timeout: float = 15.0) -> None:
        with self._lock:
            ws, self.ws = self.ws, None
            if ws:
                ws.close()
            proc, self.proc = self.proc, None
            if not proc or proc.poll() is not None:
                return
            deadline = time.monotonic() + (0 if force else timeout)
            while time.monotonic() < deadline and proc.poll() is None:
                # Ask OBS to close (WM_CLOSE to its main window, like clicking Exit).
                # taskkill without /f doesn't work: it hits Qt's helper windows first.
                close_main_window(proc.pid)
                try:
                    proc.wait(5)
                except subprocess.TimeoutExpired:
                    pass
            if proc.poll() is None:
                proc.kill()  # --disable-shutdown-check: no "crash detected" prompt next time
                self.log("[engine] OBS did not close in time; stopped it")
            else:
                self.log("[engine] OBS closed")

    def request(self, kind: str, data: dict | None = None, timeout: float | None = None) -> dict:
        ws = self.ws
        if not ws or ws.closed:
            raise ObsWsError("engine not running")
        return ws.request(kind, data, timeout)

    def _on_event(self, kind: str, data: dict) -> None:
        self.events.put((kind, data))

    def _on_close(self) -> None:
        self.events.put(("EngineClosed", {}))


# ---- the controller's view of OBS ---------------------------------------------------
class EngineObs:
    """Implements the controller's ``ObsApi`` on top of the hidden OBS."""

    def __init__(self, engine: Engine, options: Callable[[], dict], log: Callable[[str], None] = print) -> None:
        self.engine = engine
        self.options = options  # current engine options (folder, quality, audio, ...)
        self.log = log
        self.before_start: Callable[[], None] = lambda: None  # set to controller.on_output_starting
        self.recording = False
        self.replay = False
        self._spec: str | None = None
        self._audio: str | None = None
        self._last_recording: str | None = None
        self._last_replay: str | None = None
        self.active_path: str | None = None  # the file being recorded right now
        self.idle_since = time.monotonic()
        self.game_name = ""

    # state, updated from engine events by handle_event()
    def handle_event(self, kind: str, data: dict) -> str | None:
        """Returns "recording_started", "recording_stopped", "replay_saved" or "replay_stopped"."""
        state = data.get("outputState", "")
        if kind == "RecordStateChanged":
            if state == "OBS_WEBSOCKET_OUTPUT_STARTED":
                self.recording = True
                self.active_path = data.get("outputPath")
                return "recording_started"
            if state == "OBS_WEBSOCKET_OUTPUT_STOPPED":
                self.recording = False
                self.active_path = None
                self._last_recording = data.get("outputPath") or self._last_recording
                self.idle_since = time.monotonic()
                return "recording_stopped"
        elif kind == "ReplayBufferStateChanged":
            if state == "OBS_WEBSOCKET_OUTPUT_STARTED":
                self.replay = True
            elif state == "OBS_WEBSOCKET_OUTPUT_STOPPED":
                self.replay = False
                self.idle_since = time.monotonic()
                return "replay_stopped"
        elif kind == "ReplayBufferSaved":
            self._last_replay = data.get("savedReplayPath")
            return "replay_saved"
        elif kind == "EngineClosed":
            was_recording, self.recording, self.replay = self.recording, False, False
            self.active_path = None
            self.idle_since = time.monotonic()
            if was_recording:
                self.log("[engine] OBS closed while recording")
                return "recording_stopped"
        return None

    def _ensure(self) -> None:
        if not self.engine.running:
            opts = self.options()
            if self._spec is not None:
                opts["window_spec"] = self._spec
            if self._audio:
                opts["audio"] = self._audio
            Path(opts["rec_dir"]).mkdir(parents=True, exist_ok=True)
            self.engine.start(opts)
        else:
            try:
                rec_dir = str(self.options()["rec_dir"])
                Path(rec_dir).mkdir(parents=True, exist_ok=True)
                self.engine.request("SetRecordDirectory", {"recordDirectory": rec_dir})
            except ObsWsError as exc:
                self.log(f"[engine] could not set the recording folder: {exc}")

    # -- ObsApi --
    def recording_active(self) -> bool:
        return self.recording

    def replay_active(self) -> bool:
        return self.replay

    def start_recording(self) -> None:
        self.before_start()
        try:
            self._ensure()
            self.engine.request("StartRecord")
            self.recording = True
        except Exception as exc:
            self.log(f"[engine] could not start recording: {exc}")

    def stop_recording(self) -> None:
        try:
            out = self.engine.request("StopRecord", timeout=30)
            self._last_recording = out.get("outputPath") or self._last_recording
        except Exception as exc:
            self.log(f"[engine] could not stop recording: {exc}")

    def add_chapter(self, name: str) -> bool:
        try:
            self.engine.request("CreateRecordChapter", {"chapterName": name})
            return True
        except Exception:
            return False

    def start_replay(self) -> None:
        self.before_start()
        try:
            self._ensure()
            self.engine.request("StartReplayBuffer")
            self.replay = True
        except Exception as exc:
            self.log(f"[engine] could not start the replay buffer: {exc}")

    def stop_replay(self) -> None:
        try:
            self.engine.request("StopReplayBuffer")
        except Exception as exc:
            self.log(f"[engine] could not stop the replay buffer: {exc}")

    def save_replay(self) -> None:
        try:
            self.engine.request("SaveReplayBuffer")
        except Exception as exc:
            self.log(f"[engine] could not save the clip: {exc}")

    def set_scene(self, name: str) -> bool:
        return False

    def last_recording(self) -> str | None:
        return self._last_recording

    def last_replay(self) -> str | None:
        return self._last_replay

    def prepare_capture(self, window_spec: str, audio: str) -> bool:
        self._spec, self._audio = window_spec, audio
        if self.engine.running:
            self.update_capture(window_spec)
            for name, muted in audio_mutes(audio).items():
                try:
                    self.engine.request("SetInputMute", {"inputName": name, "inputMuted": muted})
                except ObsWsError:
                    pass
        return True

    def update_capture(self, window_spec: str) -> None:
        self._spec = window_spec
        if not self.engine.running:
            return
        for name in (GAME_SOURCE, WINDOW_SOURCE, AUDIO_SOURCE):
            try:
                self.engine.request("SetInputSettings", {"inputName": name, "inputSettings": {"window": window_spec}})
            except ObsWsError as exc:
                self.log(f"[engine] could not update {name}: {exc}")

    def restore_capture(self) -> None:
        self._spec = None

    def apply_quality(self, prefs: dict) -> dict:
        o = self.options()
        w, h = o["size"]
        return {"mode": "OBSPlayed", "encoder": ENCODER_NAMES.get(o["encoder"], o["encoder"]),
                "resolution": f"{w}x{h}", "fps": o["fps"], "restart_required": False}


def engine_options(prefs: dict, rec_dir: Path, obs_dir: Path | None, pids: list[int] | None = None,
                   encoder: str | None = None) -> dict:
    return {
        "rec_dir": rec_dir,
        "encoder": encoder or pick_encoder(obs_dir),
        "quality": prefs.get("quality", "high"),
        "fps": int(prefs.get("fps") or 60),
        "size": monitor_size(pids),
        "capture": prefs.get("capture", "game"),
        "audio": prefs.get("audio", "game"),
        "mic_mono": bool(prefs.get("mic_mono", True)),
        "replay_seconds": int(prefs.get("clip_length") or 30),
        "window_spec": "",
    }
