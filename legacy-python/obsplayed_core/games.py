"""Known games and process-based game detection."""
from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Game:
    name: str
    exes: tuple[str, ...]
    provider: str | None = None  # "lol", "cs2", "dota2" or None (manual markers only)


KNOWN_GAMES: tuple[Game, ...] = (
    Game("League of Legends", ("league of legends.exe",), "lol"),
    Game("Counter-Strike 2", ("cs2.exe",), "cs2"),
    Game("Dota 2", ("dota2.exe",), "dota2"),
    Game("VALORANT", ("valorant-win64-shipping.exe",)),
    Game("Fortnite", ("fortniteclient-win64-shipping.exe",)),
    Game("Apex Legends", ("r5apex.exe", "r5apex_dx12.exe")),
    Game("Overwatch 2", ("overwatch.exe",)),
    Game("Rocket League", ("rocketleague.exe",)),
    Game("Call of Duty", ("cod.exe", "cod24-cod.exe", "blackops6.exe")),
    Game("Rainbow Six Siege", ("rainbowsix.exe", "rainbowsix_vulkan.exe", "rainbowsix_be.exe")),
    Game("PUBG: Battlegrounds", ("tslgame.exe",)),
    Game("Marvel Rivals", ("marvel-win64-shipping.exe",)),
    Game("Deadlock", ("deadlock.exe", "project8.exe")),
    Game("Escape from Tarkov", ("escapefromtarkov.exe",)),
    Game("Minecraft", ("minecraft.windows.exe",)),
)


def parse_custom_games(text: str) -> list[Game]:
    """Parse user lines like ``mygame.exe = My Game`` (name optional)."""
    games: list[Game] = []
    for raw in (text or "").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        exe, _, name = line.partition("=")
        exe = exe.strip().lower()
        if not exe:
            continue
        if not exe.endswith(".exe"):
            exe += ".exe"
        games.append(Game(name.strip() or exe[:-4], (exe,)))
    return games


def detect_game(running: set[str], games: list[Game] | tuple[Game, ...]) -> Game | None:
    for game in games:
        if any(exe in running for exe in game.exes):
            return game
    return None
