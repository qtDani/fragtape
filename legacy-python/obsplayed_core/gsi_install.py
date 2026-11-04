"""Write Game State Integration config files into CS2 / Dota 2 installs."""
from __future__ import annotations

import os
import re
import sys
from pathlib import Path

CFG_NAME = "gamestate_integration_obsplayed.cfg"

CS2_CFG = """"OBSPlayed"
{{
    "uri"       "http://127.0.0.1:{port}/"
    "timeout"   "5.0"
    "buffer"    "0.0"
    "throttle"  "0.1"
    "heartbeat" "30.0"
    "auth" {{ "token" "{token}" }}
    "data"
    {{
        "provider"           "1"
        "map"                "1"
        "round"              "1"
        "player_id"          "1"
        "player_state"       "1"
        "player_match_stats" "1"
    }}
}}
"""

DOTA2_CFG = """"OBSPlayed"
{{
    "uri"       "http://127.0.0.1:{port}/"
    "timeout"   "5.0"
    "buffer"    "0.0"
    "throttle"  "0.1"
    "heartbeat" "30.0"
    "auth" {{ "token" "{token}" }}
    "data"
    {{
        "provider" "1"
        "map"      "1"
        "player"   "1"
        "hero"     "1"
    }}
}}
"""

TARGETS = {
    "Counter-Strike 2": (("Counter-Strike Global Offensive", "game", "csgo", "cfg"), CS2_CFG),
    "Dota 2": (("dota 2 beta", "game", "dota", "cfg", "gamestate_integration"), DOTA2_CFG),
}


def steam_libraries() -> list[Path]:
    roots: list[Path] = []
    if sys.platform == "win32":
        import winreg

        for hive, key in (
            (winreg.HKEY_CURRENT_USER, r"Software\Valve\Steam"),
            (winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam"),
        ):
            try:
                with winreg.OpenKey(hive, key) as k:
                    for value in ("SteamPath", "InstallPath"):
                        try:
                            roots.append(Path(winreg.QueryValueEx(k, value)[0]))
                        except OSError:
                            pass
            except OSError:
                pass
    roots.append(Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Steam")

    libs: list[Path] = []
    for root in roots:
        vdf = root / "steamapps" / "libraryfolders.vdf"
        if vdf.is_file():
            for m in re.finditer(r'"path"\s+"([^"]+)"', vdf.read_text(encoding="utf-8", errors="ignore")):
                libs.append(Path(m.group(1).replace("\\\\", "\\")))
        libs.append(root)
    seen: set[str] = set()
    unique = []
    for lib in libs:
        key = str(lib).lower()
        if key not in seen and (lib / "steamapps").is_dir():
            seen.add(key)
            unique.append(lib)
    return unique


def install(port: int, token: str) -> list[str]:
    """Install configs for every game found. Returns human-readable results."""
    results: list[str] = []
    for game, (parts, template) in TARGETS.items():
        written = False
        for lib in steam_libraries():
            game_dir = lib / "steamapps" / "common" / parts[0]
            if not game_dir.is_dir():
                continue
            cfg_dir = game_dir.joinpath(*parts[1:])
            cfg_dir.mkdir(parents=True, exist_ok=True)
            (cfg_dir / CFG_NAME).write_text(template.format(port=port, token=token), encoding="utf-8")
            results.append(f"{game}: wrote {cfg_dir / CFG_NAME}")
            written = True
        if not written:
            results.append(f"{game}: not installed, skipped")
    return results
