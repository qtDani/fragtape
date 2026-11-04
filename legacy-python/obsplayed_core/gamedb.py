"""Online game database: Discord's public list of detectable games.

https://discord.com/api/v9/applications/detectable is what Discord uses to show
"Playing ..." and lists ~10k Windows games with their executable names. We
download it in the background, keep a slim copy on disk and refresh it weekly.
"""
from __future__ import annotations

import json
import os
import threading
import time
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable

URL = "https://discord.com/api/v9/applications/detectable"
MAX_AGE = 7 * 24 * 3600
CACHE_VERSION = 2

# Executable names that many unrelated games share, or that aren't games.
# They only match when the database also gives a folder (e.g. "elden ring/game.exe").
GENERIC_EXES = {
    "game.exe", "launcher.exe", "client.exe", "engine.exe", "start.exe", "play.exe", "setup.exe",
    "javaw.exe", "java.exe", "nw.exe", "electron.exe", "unity.exe", "ue4game.exe", "main.exe",
    "app.exe", "run.exe", "autorun.exe", "dosbox.exe", "hl2.exe", "python.exe", "pythonw.exe",
    "chrome.exe", "msedge.exe", "firefox.exe", "explorer.exe", "steam.exe", "obs64.exe", "aseprite.exe",
}


@dataclass(frozen=True)
class DbGame:
    id: str
    name: str
    icon: str | None
    exes: tuple[str, ...]  # lower-case, "/" separators, may include parent folders
    skus: tuple[str, ...] = ()  # store ids like "steam:730", "epic:fn"

    @property
    def icon_url(self) -> str | None:
        return f"https://cdn.discordapp.com/app-icons/{self.id}/{self.icon}.png?size=64" if self.icon else None


def slim(raw: list[dict]) -> list[dict]:
    """Keep only what we need from the Discord payload (13 MB -> ~1 MB)."""
    out = []
    for app in raw:
        exes = [
            e["name"].lower().replace("\\", "/")
            for e in app.get("executables") or []
            if e.get("os") == "win32" and not e.get("is_launcher") and e.get("name") and not e["name"].startswith(">")
        ]
        if exes and app.get("name"):
            skus = sorted({f"{s['distributor']}:{s['id']}".lower() for s in app.get("third_party_skus") or []
                           if s.get("distributor") and s.get("id")})
            out.append({"id": str(app["id"]), "name": app["name"], "icon": app.get("icon_hash") or app.get("icon"),
                        "exes": exes, "skus": skus})
    return out


class GameDB:
    def __init__(self, cache_dir: str | os.PathLike, log: Callable[[str], None] = print,
                 fetch: Callable[[], list[dict]] | None = None) -> None:
        self.path = Path(cache_dir) / "gamedb.json"
        self.log = log
        self.fetch = fetch or _fetch
        self._lock = threading.Lock()
        self.games: list[DbGame] = []
        self._by_exe: dict[str, list[tuple[str, DbGame]]] = {}
        self._by_id: dict[str, DbGame] = {}
        self._by_name: dict[str, DbGame] = {}
        self._by_sku: dict[str, DbGame] = {}

    # ---- loading ---------------------------------------------------------
    @property
    def ready(self) -> bool:
        return bool(self.games)

    def load_cached(self) -> bool:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return False
        if not isinstance(data, dict) or data.get("v") != CACHE_VERSION:
            return False
        self._index(data["games"])
        return True

    def stale(self) -> bool:
        try:
            return time.time() - self.path.stat().st_mtime > MAX_AGE
        except OSError:
            return True

    def refresh(self) -> None:
        try:
            data = slim(self.fetch())
        except Exception as exc:
            self.log(f"[gamedb] download failed: {exc}")
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".tmp")
        tmp.write_text(json.dumps({"v": CACHE_VERSION, "games": data}, separators=(",", ":")), encoding="utf-8")
        os.replace(tmp, self.path)
        self._index(data)
        self.log(f"[gamedb] downloaded {len(self.games)} games")

    def start(self, on_ready: Callable[[], None] | None = None) -> None:
        """Load the cache now; download in the background if missing or old.

        ``on_ready`` runs on a worker thread once the list is available.
        """
        if self.load_cached():
            self.log(f"[gamedb] {len(self.games)} games loaded")
        need_download = not self.ready or self.stale()

        def work() -> None:
            from .procwatch import lower_thread_priority
            lower_thread_priority()
            if need_download:
                self.refresh()
            if self.ready and on_ready:
                try:
                    on_ready()
                except Exception as exc:
                    self.log(f"[gamedb] {exc!r}")

        threading.Thread(target=work, name="obsplayed-gamedb", daemon=True).start()

    def _index(self, data: Iterable[dict]) -> None:
        games = [DbGame(d["id"], d["name"], d.get("icon"), tuple(d["exes"]), tuple(d.get("skus", ()))) for d in data]
        by_exe: dict[str, list[tuple[str, DbGame]]] = {}
        for g in games:
            for exe in g.exes:
                by_exe.setdefault(exe.rsplit("/", 1)[-1], []).append((exe, g))
        with self._lock:
            self.games = games
            self._by_exe = by_exe
            self._by_id = {g.id: g for g in games}
            self._by_name = {g.name.lower(): g for g in games}
            self._by_sku = {s: g for g in games for s in g.skus}

    # ---- queries ---------------------------------------------------------
    def get(self, game_id: str) -> DbGame | None:
        return self._by_id.get(str(game_id))

    def by_sku(self, store: str, store_id: str) -> DbGame | None:
        return self._by_sku.get(f"{store}:{store_id}".lower())

    def by_name(self, name: str) -> DbGame | None:
        return self._by_name.get(name.lower())

    def for_exe(self, exe: str) -> DbGame | None:
        """Best guess for a bare exe name (used to find icons for built-in games)."""
        for full, g in self._by_exe.get(exe.lower(), []):
            return g
        return None

    def match(self, running) -> DbGame | None:
        """Find a running game. ``running`` is a set of exe names, optionally with ``.paths(name)``."""
        by_exe = self._by_exe
        for name in running:
            candidates = by_exe.get(name)
            if not candidates:
                continue
            paths = None
            for full, game in candidates:
                if "/" in full:
                    if paths is None:
                        paths = running.paths(name) if hasattr(running, "paths") else []
                    if any(p.endswith("/" + full) for p in paths):
                        return game
                elif name not in GENERIC_EXES and len({g.id for _, g in candidates}) == 1:
                    return game
        return None

    def search(self, query: str, limit: int = 20) -> list[DbGame]:
        q = query.strip().lower()
        if len(q) < 2:
            return []
        starts, contains = [], []
        for g in self.games:
            n = g.name.lower()
            if n.startswith(q):
                starts.append(g)
            elif q in n:
                contains.append(g)
        starts.sort(key=lambda g: len(g.name))
        contains.sort(key=lambda g: len(g.name))
        return (starts + contains)[:limit]


def _fetch() -> list[dict]:
    req = urllib.request.Request(URL, headers={"User-Agent": "OBSPlayed (OBS script)"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.loads(resp.read().decode("utf-8"))
