"""The OBSPlayed panel: a small local web app shown inside OBS as a browser dock.

Security: binds to 127.0.0.1 only, checks the Host header (DNS rebinding) and
requires a per-install secret token, sent as a header for API calls. Web pages
can't add custom headers to cross-origin requests without a CORS preflight,
which this server never approves.
"""
from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING, Callable
from urllib.parse import parse_qs, urlparse

from . import importers, modes
from .games import KNOWN_GAMES
from .procwatch import lower_thread_priority

if TYPE_CHECKING:
    from .controller import Controller
    from .gamedb import GameDB
    from .library import Library

PAGE = Path(__file__).with_name("panel.html")
KILL_DETECTION = {g.name.lower() for g in KNOWN_GAMES if g.provider}


class Panel:
    def __init__(self, library: "Library", gamedb: "GameDB | None", controller: "Callable[[], Controller | None]",
                 port: int = 3132, log=print) -> None:
        self.library = library
        self.gamedb = gamedb
        self.controller = controller
        self.port = port
        self.log = log
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        self.scanning = False
        self.page = PAGE

    def scan_async(self, force: bool = False) -> None:
        """Look for installed / previously played games (Steam, Riot, Epic).

        Runs at idle priority, and is skipped when the launcher files haven't
        changed since the last scan (unless ``force``, i.e. the Scan PC button).
        """
        if self.scanning or not (self.gamedb and self.gamedb.ready):
            return
        self.scanning = True

        def run() -> None:
            lower_thread_priority()
            try:
                sig = importers.signature()
                if not force and sig == self.library.data.get("scan_signature"):
                    self.log("[scan] nothing changed since the last scan")
                    return
                found = importers.scan(self.gamedb)
                added = self.library.merge_imported(found)
                with self.library._lock:
                    self.library.data["scan_signature"] = sig
                    self.library._save()
                self.log(f"[scan] found {len(found)} games on this PC, {added} new")
            except Exception as exc:
                self.log(f"[scan] failed: {exc!r}")
            finally:
                self.scanning = False

        threading.Thread(target=run, name="obsplayed-scan", daemon=True).start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/?t={self.library.token}"

    # ---- API -------------------------------------------------------------
    def _icon(self, g: dict) -> str | None:
        if g.get("discord_id") and g.get("icon"):
            return f"https://cdn.discordapp.com/app-icons/{g['discord_id']}/{g['icon']}.png?size=64"
        db = self.gamedb
        if db and db.ready:
            hit = db.by_name(g["name"]) or next((db.for_exe(e) for e in g.get("exes", []) if db.for_exe(e)), None)
            if hit:
                return hit.icon_url
        return None

    def state(self) -> dict:
        c = self.controller()
        games = []
        for g in self.library.games():
            games.append({
                "name": g["name"],
                "mode": g.get("mode", modes.ASK),
                "sessions": g.get("sessions", 0),
                "playtime": g.get("playtime", 0),
                "last_played": g.get("last_played"),
                "source": g.get("source"),
                "exes": g.get("exes", []),
                "icon": self._icon(g),
                "kills": g["name"].lower() in KILL_DETECTION,
                "installed": g.get("installed"),
                "added": g.get("added"),
            })
        db = self.gamedb
        return {
            "status": dict(c.status) if c else {},
            "default_mode": self.library.default_mode,
            "prefs": self.library.prefs(),
            "games": games,
            "db": {"ready": bool(db and db.ready), "count": len(db.games) if db else 0},
            "scanning": self.scanning,
            "page_rev": int(self.page.stat().st_mtime),
        }

    def search(self, q: str) -> list[dict]:
        if not self.gamedb:
            return []
        have = {g["name"].lower() for g in self.library.games()}
        return [{"id": g.id, "name": g.name, "icon": g.icon_url, "added": g.name.lower() in have,
                 "kills": g.name.lower() in KILL_DETECTION} for g in self.gamedb.search(q)]

    def post(self, path: str, body: dict) -> dict:
        lib = self.library
        if path == "/api/mode":
            lib.set_mode(str(body["name"]), str(body["mode"]))
        elif path == "/api/default":
            lib.set_default_mode(str(body["mode"]))
        elif path == "/api/remove":
            lib.remove(str(body["name"]))
        elif path == "/api/scan":
            self.scan_async(force=True)
        elif path == "/api/pref":
            lib.set_pref(str(body["key"]), body["value"])
            c = self.controller()
            if c:
                c.prefs_changed()
        elif path == "/api/answer":
            c = self.controller()
            if c:
                c.answer(str(body["name"]), str(body["mode"]))
        elif path == "/api/add":
            mode = str(body.get("mode") or modes.ASK)
            if body.get("id"):
                g = self.gamedb.get(str(body["id"])) if self.gamedb else None
                if not g:
                    raise ValueError("unknown game")
                lib.add(g.name, source="discord", discord_id=g.id, icon=g.icon, exes=list(g.exes))
                lib.set_mode(g.name, mode)
            else:
                exe = str(body.get("exe") or "").strip().lower()
                if not exe:
                    raise ValueError("exe required")
                if not exe.endswith(".exe"):
                    exe += ".exe"
                name = str(body.get("name") or "").strip() or exe[:-4]
                lib.add(name, source="custom", exes=[exe])
                lib.set_mode(name, mode)
        else:
            return self.post_extra(path, body)
        return {"ok": True}

    # ---- extension points (the standalone app adds clips, media, ...) ------------
    def post_extra(self, path: str, body: dict) -> dict:
        raise KeyError(path)

    def get_extra(self, handler, path: str, query: dict) -> bool:
        """Handle a GET; return False if the path is unknown."""
        return False

    def post_raw(self, path: str, query: dict, data: bytes) -> dict:
        raise KeyError(path)

    # ---- server ------------------------------------------------------------
    def start(self) -> None:
        if self._server:
            return
        panel = self

        class Handler(BaseHTTPRequestHandler):
            def _host_ok(self) -> bool:
                return self.headers.get("Host", "") in (f"127.0.0.1:{panel.port}", f"localhost:{panel.port}")

            def _token_ok(self, allow_query: bool = False) -> bool:
                if self.headers.get("X-OBSPlayed-Token") == panel.library.token:
                    return True
                # <video>/<img> can't send headers: media URLs carry the token instead.
                return allow_query and parse_qs(urlparse(self.path).query).get("t", [""])[0] == panel.library.token

            def _send(self, code: int, body: bytes, ctype: str = "application/json") -> None:
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _json(self, data, code: int = 200) -> None:
                self._send(code, json.dumps(data).encode("utf-8"))

            def do_GET(self) -> None:  # noqa: N802
                if not self._host_ok():
                    return self._send(403, b"forbidden", "text/plain")
                url = urlparse(self.path)
                if url.path == "/":
                    if parse_qs(url.query).get("t", [""])[0] != panel.library.token:
                        return self._send(403, b"Open the panel from OBS (the link in the script settings).", "text/plain")
                    html = panel.page.read_text(encoding="utf-8").replace("__TOKEN__", panel.library.token)
                    return self._send(200, html.encode("utf-8"), "text/html; charset=utf-8")
                if not self._token_ok(allow_query=not url.path.startswith("/api/")):
                    return self._json({"error": "forbidden"}, 403)
                if url.path == "/api/state":
                    return self._json(panel.state())
                if url.path == "/api/search":
                    return self._json(panel.search(parse_qs(url.query).get("q", [""])[0]))
                try:
                    if panel.get_extra(self, url.path, parse_qs(url.query)):
                        return
                except (ConnectionError, OSError):
                    return  # the browser stopped reading (seeking in a video)
                self._json({"error": "not found"}, 404)

            def do_POST(self) -> None:  # noqa: N802
                if not (self._host_ok() and self._token_ok()):
                    return self._json({"error": "forbidden"}, 403)
                try:
                    url = urlparse(self.path)
                    if not self.headers.get("Content-Type", "").startswith(("image/", "application/octet-stream")):
                        length = min(int(self.headers.get("Content-Length") or 0), 100_000)
                        body = json.loads(self.rfile.read(length) or b"{}")
                        self._json(panel.post(url.path, body))
                    else:
                        length = int(self.headers.get("Content-Length") or 0)
                        if length > 4_000_000:
                            raise ValueError("too large")
                        self._json(panel.post_raw(url.path, parse_qs(url.query), self.rfile.read(length)))
                except KeyError:
                    self._json({"error": "not found"}, 404)
                except Exception as exc:
                    self._json({"error": str(exc)}, 400)

            def log_message(self, *args) -> None:
                pass

        try:
            self._server = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        except OSError as exc:
            self.log(f"[panel] could not listen on 127.0.0.1:{self.port}: {exc}")
            return
        self._server.daemon_threads = True
        self._thread = threading.Thread(target=self._server.serve_forever, name="obsplayed-panel", daemon=True)
        self._thread.start()
        self.log(f"[panel] {self.url}")

    def stop(self) -> None:
        if self._server:
            self._server.shutdown()
            self._server.server_close()
        if self._thread:
            self._thread.join(timeout=3)
        self._server = None
        self._thread = None
