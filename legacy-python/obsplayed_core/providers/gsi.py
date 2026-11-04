"""Valve Game State Integration (CS2 and Dota 2).

The game POSTs JSON state to a local HTTP endpoint configured by a
``gamestate_integration_*.cfg`` file (see ``gsi_install.py``). We diff the
local player's stats between updates to detect kills, deaths and so on.
"""
from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from ..events import EventBus, GameEvent

CS2_APPID = 730
DOTA2_APPID = 570


class CS2Tracker:
    GAME = "Counter-Strike 2"
    ROUND_MULTI = {3: "triple_kill", 4: "quad_kill", 5: "ace"}

    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        self.map: str | None = None
        self.stats: dict | None = None
        self.round_hs = 0
        self.round_kills = 0

    def update(self, payload: dict) -> list[GameEvent]:
        me = (payload.get("provider") or {}).get("steamid")
        player = payload.get("player") or {}
        # When you're dead the game reports the player you're spectating.
        if not me or player.get("steamid") != me:
            return []
        stats = player.get("match_stats")
        state = player.get("state") or {}
        if not isinstance(stats, dict):
            return []
        mapname = (payload.get("map") or {}).get("name")
        cur = {k: int(stats.get(k) or 0) for k in ("kills", "deaths", "mvps")}
        round_kills = int(state.get("round_kills") or 0)
        round_hs = int(state.get("round_killhs") or 0)

        if mapname != self.map or self.stats is None or cur["kills"] < self.stats["kills"]:
            # New map / new match / warmup reset: take a fresh baseline.
            self.map, self.stats, self.round_kills, self.round_hs = mapname, cur, round_kills, round_hs
            return []

        out: list[GameEvent] = []
        new_kills = cur["kills"] - self.stats["kills"]
        if round_kills < self.round_kills:
            self.round_kills, self.round_hs = 0, 0
        new_hs = max(0, round_hs - self.round_hs)
        for i in range(min(new_kills, 5)):
            out.append(GameEvent("headshot" if i < new_hs else "kill", self.GAME))
        for n in range(self.round_kills + 1, round_kills + 1):
            if n in self.ROUND_MULTI:
                out.append(GameEvent(self.ROUND_MULTI[n], self.GAME, f"{n} kills this round"))
        if cur["deaths"] > self.stats["deaths"]:
            out.append(GameEvent("death", self.GAME))
        if cur["mvps"] > self.stats["mvps"]:
            out.append(GameEvent("mvp", self.GAME, "round MVP"))

        self.stats, self.round_kills, self.round_hs = cur, round_kills, round_hs
        return out


class Dota2Tracker:
    GAME = "Dota 2"

    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        self.match: str | None = None
        self.stats: dict | None = None

    def update(self, payload: dict) -> list[GameEvent]:
        player = payload.get("player") or {}
        if "kills" not in player:  # spectating / not in a match
            return []
        match = str((payload.get("map") or {}).get("matchid") or "")
        cur = {k: int(player.get(k) or 0) for k in ("kills", "deaths", "assists")}
        if match != self.match or self.stats is None:
            self.match, self.stats = match, cur
            return []
        out: list[GameEvent] = []
        for _ in range(max(0, min(cur["kills"] - self.stats["kills"], 5))):
            out.append(GameEvent("kill", self.GAME))
        if cur["deaths"] > self.stats["deaths"]:
            out.append(GameEvent("death", self.GAME))
        for _ in range(max(0, min(cur["assists"] - self.stats["assists"], 5))):
            out.append(GameEvent("assist", self.GAME))
        self.stats = cur
        return out


class GSIServer:
    def __init__(self, bus: EventBus, port: int = 3131, token: str = "obsplayed", log=print) -> None:
        self.bus = bus
        self.port = port
        self.token = token
        self.log = log
        self.trackers = {CS2_APPID: CS2Tracker(), DOTA2_APPID: Dota2Tracker()}
        self._lock = threading.Lock()
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    def handle(self, payload: dict) -> None:
        auth = (payload.get("auth") or {}).get("token")
        if self.token and auth != self.token:
            return
        appid = (payload.get("provider") or {}).get("appid")
        tracker = self.trackers.get(int(appid or 0))
        if not tracker:
            return
        with self._lock:
            events = tracker.update(payload)
        for e in events:
            self.bus.emit(e)

    def start(self) -> None:
        if self._server:
            return
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(min(length, 1_000_000))
                self.send_response(200)
                self.end_headers()
                try:
                    owner.handle(json.loads(body.decode("utf-8")))
                except Exception as exc:  # never let a bad payload kill the server
                    owner.log(f"[gsi] bad payload: {exc}")

            def log_message(self, *args) -> None:
                pass

        try:
            self._server = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        except OSError as exc:
            self.log(f"[gsi] could not listen on 127.0.0.1:{self.port}: {exc}")
            return
        self._server.daemon_threads = True
        self._thread = threading.Thread(target=self._server.serve_forever, name="obsplayed-gsi", daemon=True)
        self._thread.start()
        self.log(f"[gsi] listening on http://127.0.0.1:{self.port}/")

    def stop(self) -> None:
        if self._server:
            self._server.shutdown()
            self._server.server_close()
        if self._thread:
            self._thread.join(timeout=3)
        self._server = None
        self._thread = None
