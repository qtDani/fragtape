"""Find games already on this PC (Steam, Riot, Epic) to fill the library.

Steam also tells us every game you've played on this account, with play time,
from ``userdata/<id>/config/localconfig.vdf``. Steam app ids are turned into
names through the game database (Discord's list links games to store ids).
"""
from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import TYPE_CHECKING

from .gamedb import GENERIC_EXES
from .games import KNOWN_GAMES
from .gsi_install import steam_libraries

BUILTIN_NAMES = {g.name for g in KNOWN_GAMES}

if TYPE_CHECKING:
    from .gamedb import DbGame, GameDB

_TOKEN = re.compile(r'"((?:\\.|[^"\\])*)"|([{}])')
NOT_GAME_EXE = re.compile(
    r"(^|/)(support|redist\w*|_commonredist|commonredist|installers?|directx|vcredist|easyanticheat|battleye|tools?)/"
    r"|setup|install|crash|cleaner|uninst|report|updater|helper", re.I)
NOT_GAMES = re.compile(r"redistributable|steamworks|soundtrack|dedicated server|\bsdk\b|benchmark|steamvr", re.I)


def parse_vdf(text: str) -> dict:
    """Minimal parser for Valve's KeyValues text format."""
    root: dict = {}
    stack = [root]
    key = None
    for m in _TOKEN.finditer(text):
        s, brace = m.groups()
        if brace == "{":
            child: dict = {}
            stack[-1][key if key is not None else ""] = child
            stack.append(child)
            key = None
        elif brace == "}":
            if len(stack) > 1:
                stack.pop()
            key = None
        elif key is None:
            key = s.replace('\\"', '"').replace("\\\\", "\\")
        else:
            stack[-1][key] = s.replace('\\"', '"').replace("\\\\", "\\")
            key = None
    return root


def _ci(d: dict, *path: str):
    """Case-insensitive nested lookup."""
    for p in path:
        if not isinstance(d, dict):
            return None
        d = next((v for k, v in d.items() if k.lower() == p.lower()), None)
    return d


def _entry(db_game: "DbGame | None", name: str, source: str, **info) -> dict:
    """Build a library entry, preferring the built-in name so detection matches it."""
    exes = list(db_game.exes) if db_game else []
    builtin = next((g for g in KNOWN_GAMES if set(g.exes) & {e.rsplit("/", 1)[-1] for e in exes}), None)
    if builtin is None:
        builtin = next((g for g in KNOWN_GAMES if g.name.lower() == name.lower()), None)
    out = {
        "name": builtin.name if builtin else (db_game.name if db_game else name),
        "source": source,
        "exes": [e for e in exes if "/" in e or e not in GENERIC_EXES],
        **{k: v for k, v in info.items() if v is not None},
    }
    if db_game:
        out["discord_id"] = db_game.id
        out["icon"] = db_game.icon
    return out


def game_exes_in(folder: Path, db: "GameDB") -> list[str]:
    """Executables in an install folder that are known game exes, as "<folder>/<relative path>".

    Including the folder makes detection exact, even when the store id points
    at another edition of the game (e.g. GTA V Legacy vs Enhanced).
    """
    known = {e for g in KNOWN_GAMES for e in g.exes}
    out: list[str] = []
    try:
        level = [(folder, "")]
        for _depth in range(3):
            nxt = []
            for path, rel in level:
                with os.scandir(path) as it:
                    for entry in it:
                        name = entry.name.lower()
                        if entry.is_dir(follow_symlinks=False) and _depth < 2:
                            nxt.append((entry.path, f"{rel}{name}/"))
                        elif (name.endswith(".exe") and name not in GENERIC_EXES and not NOT_GAME_EXE.search(rel + name)
                              and (name in known or db.for_exe(name))):
                            out.append(f"{folder.name.lower()}/{rel}{name}")
            level = nxt
    except OSError:
        pass
    return out


def steam_games(db: "GameDB", libraries: list[Path] | None = None) -> list[dict]:
    libraries = libraries if libraries is not None else steam_libraries()
    played: dict[str, dict] = {}
    installed: dict[str, str] = {}
    folders: dict[str, Path] = {}
    for lib in libraries:
        for acf in (lib / "steamapps").glob("appmanifest_*.acf"):
            try:
                st = _ci(parse_vdf(acf.read_text(encoding="utf-8", errors="ignore")), "AppState") or {}
            except OSError:
                continue
            if st.get("appid") and st.get("name"):
                installed[st["appid"]] = st["name"]
                if st.get("installdir"):
                    folders[st["appid"]] = lib / "steamapps" / "common" / st["installdir"]
        for cfg in (lib / "userdata").glob("*/config/localconfig.vdf"):
            try:
                apps = _ci(parse_vdf(cfg.read_text(encoding="utf-8", errors="ignore")),
                           "UserLocalConfigStore", "Software", "Valve", "Steam", "apps") or {}
            except OSError:
                continue
            for appid, a in apps.items():
                if not isinstance(a, dict):
                    continue
                last = int(a.get("LastPlayed") or 0)
                minutes = int(a.get("Playtime") or 0)
                if last or minutes:
                    p = played.setdefault(appid, {"last": 0, "minutes": 0})
                    p["last"] = max(p["last"], last)
                    p["minutes"] = max(p["minutes"], minutes)

    out = []
    for appid in set(played) | set(installed):
        name = installed.get(appid)
        if name and NOT_GAMES.search(name):
            continue
        game = db.by_sku("steam", appid)
        if not game and not name:
            continue  # played once but uninstalled and unknown: no name to show
        p = played.get(appid, {})
        e = _entry(game, name or game.name, "steam", steam_appid=appid,
                   installed=appid in installed,
                   last_played=p.get("last") or None,
                   playtime=(p.get("minutes") or 0) * 60)
        if appid in folders:
            e["exes"] = sorted(set(e["exes"]) | set(game_exes_in(folders[appid], db)))
            if name and e["name"] not in BUILTIN_NAMES:
                # Use Steam's name for the edition that's actually installed.
                e["name"] = re.sub(r"[®™]", "", name).strip()
        out.append(e)
    return out


def riot_games(db: "GameDB") -> list[dict]:
    path = Path(os.environ.get("ProgramData", r"C:\ProgramData")) / "Riot Games" / "RiotClientInstalls.json"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    out = []
    for folder in (data.get("associated_client") or {}):
        low = folder.lower()
        for key, name in (("league of legends", "League of Legends"), ("valorant", "VALORANT"),
                          ("legends of runeterra", "Legends of Runeterra")):
            if key in low:
                out.append(_entry(db.by_name(name), name, "riot", installed=Path(folder).exists()))
    return out


def epic_games(db: "GameDB") -> list[dict]:
    folder = Path(os.environ.get("ProgramData", r"C:\ProgramData")) / "Epic" / "EpicGamesLauncher" / "Data" / "Manifests"
    out = []
    for item in folder.glob("*.item"):
        try:
            m = json.loads(item.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        name = m.get("DisplayName")
        if not name or NOT_GAMES.search(name):
            continue
        exe = (m.get("LaunchExecutable") or "").replace("\\", "/").rsplit("/", 1)[-1].lower()
        game = db.by_name(name) or (db.for_exe(exe) if exe and exe not in GENERIC_EXES else None)
        e = _entry(game, name, "epic", installed=True)
        if exe and exe not in GENERIC_EXES and exe not in e["exes"]:
            e["exes"].append(exe)
        out.append(e)
    return out


def merge(entries: list[dict]) -> list[dict]:
    """Combine entries with the same name (e.g. a game and its test server)."""
    found: dict[str, dict] = {}
    for e in entries:
        key = e["name"].lower()
        if key not in found:
            found[key] = dict(e, exes=list(e.get("exes", [])))
            continue
        f = found[key]
        f["exes"] = sorted(set(f["exes"]) | set(e.get("exes", [])))
        f["installed"] = bool(f.get("installed") or e.get("installed"))
        f["last_played"] = max(f.get("last_played") or 0, e.get("last_played") or 0) or None
        f["playtime"] = max(f.get("playtime") or 0, e.get("playtime") or 0)
        if e.get("installed"):
            # The installed edition wins for the store id and icon.
            for k in ("steam_appid", "discord_id", "icon"):
                if e.get(k):
                    f[k] = e[k]
    return list(found.values())


def signature() -> str:
    """Cheap fingerprint of the launcher files a scan reads (a few stat() calls).

    If it hasn't changed since the last scan, there's nothing new to find.
    """
    files: list[Path] = []
    for lib in steam_libraries():
        files.append(lib / "steamapps")  # folder mtime changes when a game is (un)installed
        files.extend((lib / "userdata").glob("*/config/localconfig.vdf"))
    pd = Path(os.environ.get("ProgramData", r"C:\ProgramData"))
    files.append(pd / "Riot Games" / "RiotClientInstalls.json")
    files.append(pd / "Epic" / "EpicGamesLauncher" / "Data" / "Manifests")
    parts = []
    for f in files:
        try:
            st = f.stat()
            parts.append(f"{f}:{int(st.st_mtime)}:{st.st_size}")
        except OSError:
            pass
    return "|".join(sorted(parts))


def scan(db: "GameDB") -> list[dict]:
    entries: list[dict] = []
    for importer in (steam_games, riot_games, epic_games):
        try:
            entries.extend(importer(db))
        except Exception:
            continue
    return merge(entries)
