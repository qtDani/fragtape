"""The user's game library: games played or added, their mode and play history.

Stored as JSON in OBS's plugin_config folder. Accessed from the OBS main thread
and from the panel's HTTP threads, so every public method takes the lock.
"""
from __future__ import annotations

import json
import os
import secrets
import threading
import time
from pathlib import Path
from typing import Any

from . import modes

VALID = (modes.ASK, modes.FULL, modes.HIGHLIGHTS, modes.OFF)


def default_dir() -> Path:
    base = os.environ.get("APPDATA") or str(Path.home())
    return Path(base) / "obs-studio" / "plugin_config" / "obsplayed"


class Library:
    def __init__(self, path: str | os.PathLike | None = None) -> None:
        self.path = Path(path) if path else default_dir() / "library.json"
        self._lock = threading.RLock()
        self.data: dict[str, Any] = {"version": 1, "default_mode": modes.ASK, "games": {}}
        try:
            loaded = json.loads(self.path.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                self.data.update(loaded)
        except (OSError, ValueError):
            pass
        self.data.setdefault("token", secrets.token_urlsafe(16))
        self._save()

    # ---- persistence -----------------------------------------------------
    def _save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self.data, indent=2), encoding="utf-8")
        os.replace(tmp, self.path)

    @property
    def token(self) -> str:
        return self.data["token"]

    # ---- modes -----------------------------------------------------------
    @property
    def default_mode(self) -> str:
        return self.data.get("default_mode", modes.ASK)

    def set_default_mode(self, mode: str) -> None:
        if mode not in VALID:
            raise ValueError(mode)
        with self._lock:
            self.data["default_mode"] = mode
            self._save()

    def mode_for(self, name: str) -> str:
        """The mode for a game; ``ask`` games fall back to the default for new games."""
        with self._lock:
            g = self.data["games"].get(name.lower())
            mode = g.get("mode", modes.ASK) if g else modes.ASK
            return self.default_mode if mode == modes.ASK else mode

    def set_mode(self, name: str, mode: str, **info: Any) -> None:
        if mode not in VALID:
            raise ValueError(mode)
        with self._lock:
            self._entry(name, **info)["mode"] = mode
            self._save()

    # ---- recording preferences ---------------------------------------------
    PREF_CHOICES = {
        "capture": ("game", "scene"),         # OBSPlayed's own game scene, or the current OBS scene
        "audio": ("game", "game_mic", "all"),  # game only, game + microphone, everything
        "quality": ("keep", "small", "balanced", "high", "max"),
        "fps": (0, 30, 60),                    # 0 = keep OBS's setting
    }
    PREF_DEFAULTS = {"capture": "game", "audio": "game", "quality": "high", "fps": 0}

    def prefs(self) -> dict:
        with self._lock:
            saved = self.data.get("prefs", {})
            return {k: saved[k] if saved.get(k) in self.PREF_CHOICES[k] and type(saved[k]) is type(d) else d
                    for k, d in self.PREF_DEFAULTS.items()}

    def set_pref(self, key: str, value: Any) -> None:
        if key not in self.PREF_CHOICES:
            raise KeyError(key)
        kind = type(self.PREF_DEFAULTS[key])
        if kind is bool:
            value = value in (True, "true", 1, "1")
        elif kind is int:
            value = int(value)
        if value not in self.PREF_CHOICES[key]:
            raise ValueError(f"{key}: {value!r}")
        with self._lock:
            self.data.setdefault("prefs", {})[key] = value
            self._save()

    # ---- games -----------------------------------------------------------
    def _entry(self, name: str, **info: Any) -> dict:
        games = self.data["games"]
        g = games.get(name.lower())
        if g is None:
            g = games[name.lower()] = {
                "name": name, "mode": modes.ASK, "exes": [], "source": "custom",
                "added": int(time.time()), "sessions": 0, "playtime": 0, "last_played": None,
            }
        for key, value in info.items():
            if value is None:
                continue
            if key == "exes":
                g["exes"] = sorted(set(g.get("exes", [])) | {e.lower() for e in value})
            else:
                g[key] = value
        return g

    def add(self, name: str, **info: Any) -> dict:
        with self._lock:
            g = dict(self._entry(name, **info))
            self._save()
            return g

    def remove(self, name: str) -> None:
        with self._lock:
            self.data["games"].pop(name.lower(), None)
            self._save()

    def game_started(self, name: str, **info: Any) -> None:
        with self._lock:
            g = self._entry(name, **info)
            g["sessions"] = g.get("sessions", 0) + 1
            g["last_played"] = int(time.time())
            self._save()

    def game_ended(self, name: str, seconds: float) -> None:
        with self._lock:
            g = self.data["games"].get(name.lower())
            if g:
                g["playtime"] = int(g.get("playtime", 0) + max(0, seconds))
                self._save()

    def games(self) -> list[dict]:
        with self._lock:
            return [dict(g) for g in self.data["games"].values()]

    def custom_exes(self) -> list[tuple[str, str]]:
        """(exe, game name) pairs the user added by hand."""
        with self._lock:
            return [(e, g["name"]) for g in self.data["games"].values() for e in g.get("exes", [])]

    def merge_imported(self, entries: list[dict]) -> int:
        """Add games found on the PC. Never touches the user's chosen modes."""
        added = 0
        with self._lock:
            games = self.data["games"]
            for e in entries:
                key = e["name"].lower()
                g = games.get(key)
                if g is None:
                    added += 1
                    g = self._entry(e["name"])
                    g["source"] = e.get("source", "custom")
                    g.pop("added", None)  # sort imported games by when they were played instead
                for k in ("discord_id", "icon", "steam_appid", "installed"):
                    if e.get(k) is not None:
                        g[k] = e[k]
                if e.get("exes"):
                    g["exes"] = sorted(set(g.get("exes", [])) | set(e["exes"]))
                if e.get("last_played"):
                    g["last_played"] = max(g.get("last_played") or 0, e["last_played"])
                if e.get("playtime"):
                    g["playtime"] = max(g.get("playtime") or 0, e["playtime"])
            self.data["last_scan"] = int(time.time())
            self._save()
        return added

    @property
    def last_scan(self) -> int:
        return int(self.data.get("last_scan") or 0)

    # ---- migration from the old text settings -----------------------------
    def import_text_settings(self, game_modes: str, custom_games: str) -> int:
        from .games import parse_custom_games

        n = 0
        with self._lock:
            for game in parse_custom_games(custom_games):
                self._entry(game.name, exes=list(game.exes))
                n += 1
            for raw in (game_modes or "").splitlines():
                name, sep, mode = raw.rpartition("=")
                parsed = modes.parse_modes(raw)
                if sep and parsed:
                    self._entry(name.strip())["mode"] = next(iter(parsed.values()))
                    n += 1
            if n:
                self._save()
        return n
