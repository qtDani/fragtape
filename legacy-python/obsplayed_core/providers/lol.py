"""League of Legends: polls Riot's Live Client Data API on 127.0.0.1:2999.

The API is served by the game itself while a match is running, uses a
self-signed certificate, and only listens on localhost.
"""
from __future__ import annotations

import json
import ssl
import threading
import urllib.request

from ..events import EventBus, GameEvent

GAME = "League of Legends"
BASE = "https://127.0.0.1:2999/liveclientdata"
MULTIKILL = {2: "double_kill", 3: "triple_kill", 4: "quad_kill", 5: "penta_kill"}
OBJECTIVES = {
    "DragonKill": "Dragon",
    "BaronKill": "Baron Nashor",
    "HeraldKill": "Rift Herald",
    "HordeKill": "Voidgrub",
    "AtakhanKill": "Atakhan",
}
STRUCTURES = {"TurretKilled": "Turret", "InhibKilled": "Inhibitor"}


def _base(name: object) -> str:
    # Riot IDs look like "Name#TAG"; event payloads sometimes omit the tag.
    return str(name or "").split("#", 1)[0].strip().lower()


def _is_me(name: object, me: str) -> bool:
    n = _base(name)
    return bool(n) and n == _base(me)


def translate(event: dict, me: str) -> list[GameEvent]:
    """Map one Live Client Data event to zero or more GameEvents for player ``me``."""
    name = event.get("EventName")
    out: list[GameEvent] = []
    if name == "ChampionKill":
        if _is_me(event.get("KillerName"), me):
            out.append(GameEvent("kill", GAME, f"killed {event.get('VictimName', '?')}"))
        elif _is_me(event.get("VictimName"), me):
            out.append(GameEvent("death", GAME, f"killed by {event.get('KillerName', '?')}"))
        elif any(_is_me(a, me) for a in event.get("Assisters") or []):
            out.append(GameEvent("assist", GAME, f"assisted on {event.get('VictimName', '?')}"))
    elif name == "Multikill" and _is_me(event.get("KillerName"), me):
        kind = MULTIKILL.get(int(event.get("KillStreak") or 0))
        if kind:
            out.append(GameEvent(kind, GAME))
    elif name == "Ace" and _is_me(event.get("Acer"), me):
        out.append(GameEvent("ace", GAME))
    elif name == "FirstBlood" and _is_me(event.get("Recipient"), me):
        out.append(GameEvent("first_blood", GAME))
    elif name in OBJECTIVES:
        involved = _is_me(event.get("KillerName"), me) or any(_is_me(a, me) for a in event.get("Assisters") or [])
        if involved:
            what = OBJECTIVES[name]
            if event.get("DragonType"):
                what = f"{event['DragonType']} Dragon"
            stolen = str(event.get("Stolen", "")).lower() == "true"
            out.append(GameEvent("objective_steal" if stolen else "objective", GAME, what))
    elif name in STRUCTURES and _is_me(event.get("KillerName"), me):
        out.append(GameEvent("structure", GAME, STRUCTURES[name]))
    return out


class LeagueProvider:
    def __init__(self, bus: EventBus, log=print, interval: float = 1.0) -> None:
        self.bus = bus
        self.log = log
        self.interval = interval
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._ctx = ssl.create_default_context()
        self._ctx.check_hostname = False
        self._ctx.verify_mode = ssl.CERT_NONE

    def _get(self, path: str):
        with urllib.request.urlopen(f"{BASE}/{path}", context=self._ctx, timeout=2) as resp:
            return json.loads(resp.read().decode("utf-8"))

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, name="obsplayed-lol", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=3)
        self._thread = None

    def _run(self) -> None:
        me: str | None = None
        seen: set[int] | None = None
        while not self._stop.wait(self.interval):
            try:
                if me is None:
                    me = self._get("activeplayername")
                    self.log(f"[lol] connected, active player: {me}")
                events = self._get("eventdata").get("Events", [])
            except Exception:
                # Game still loading, or the match ended. Keep trying.
                continue
            if seen is None:
                # Skip whatever happened before we connected.
                seen = {e.get("EventID") for e in events}
                continue
            for e in events:
                eid = e.get("EventID")
                if eid in seen:
                    continue
                seen.add(eid)
                for ge in translate(e, me):
                    self.bus.emit(ge)
