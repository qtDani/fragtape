"""Tests for the standalone app parts: websocket client, engine config, clip library, prefs, tray helpers."""
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from obsplayed_core import engine, obsws  # noqa: E402
from obsplayed_core.app import AppLibrary  # noqa: E402
from obsplayed_core.clips import ClipStore, clip_id  # noqa: E402
from obsplayed_core.tray import MOD_CONTROL, make_ico, parse_hotkey  # noqa: E402


# ---- a tiny fake obs-websocket server ------------------------------------------------
class FakeObsWs:
    def __init__(self, password="pw"):
        self.password = password
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(1)
        self.port = self.sock.getsockname()[1]
        self.requests = []
        threading.Thread(target=self._serve, daemon=True).start()

    def _send(self, conn, msg):
        data = json.dumps(msg).encode()
        head = bytes([0x81]) + (bytes([len(data)]) if len(data) < 126 else bytes([126]) + struct.pack(">H", len(data)))
        conn.sendall(head + data)

    def _recv(self, conn):
        def exact(n):
            b = b""
            while len(b) < n:
                chunk = conn.recv(n - len(b))
                if not chunk:
                    raise ConnectionError
                b += chunk
            return b
        b1, b2 = exact(2)
        n = b2 & 0x7F
        if n == 126:
            n = struct.unpack(">H", exact(2))[0]
        mask = exact(4)
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(exact(n)))
        return b1 & 0x0F, data

    def _serve(self):
        conn, _ = self.sock.accept()
        req = b""
        while b"\r\n\r\n" not in req:
            req += conn.recv(4096)
        key = [l.split(b": ")[1] for l in req.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key")][0]
        accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest()).decode()
        conn.sendall(f"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                     f"Sec-WebSocket-Accept: {accept}\r\n\r\n".encode())
        self._send(conn, {"op": 0, "d": {"rpcVersion": 1, "authentication": {"salt": "s", "challenge": "c"}}})
        op, data = self._recv(conn)
        ident = json.loads(data)["d"]
        if ident.get("authentication") != obsws.auth_string(self.password, "s", "c"):
            conn.close()
            return
        self._send(conn, {"op": 2, "d": {"negotiatedRpcVersion": 1}})
        self._send(conn, {"op": 5, "d": {"eventType": "RecordStateChanged", "eventData": {"outputState": "X"}}})
        try:
            while True:
                op, data = self._recv(conn)
                if op == 8:
                    return
                d = json.loads(data)["d"]
                self.requests.append(d)
                ok = d["requestType"] != "Fail"
                self._send(conn, {"op": 7, "d": {"requestType": d["requestType"], "requestId": d["requestId"],
                                                 "requestStatus": {"result": ok, "code": 100 if ok else 600, "comment": None if ok else "nope"},
                                                 "responseData": {"echo": d.get("requestData")}}})
        except (ConnectionError, OSError):
            pass


class ObsWsTests(unittest.TestCase):
    def test_auth_string_matches_spec(self):
        # obs-websocket: base64(sha256(base64(sha256(password + salt)) + challenge))
        secret = base64.b64encode(hashlib.sha256(b"pwsalt").digest()).decode()
        expected = base64.b64encode(hashlib.sha256((secret + "chal").encode()).digest()).decode()
        self.assertEqual(obsws.auth_string("pw", "salt", "chal"), expected)

    def test_frame_lengths(self):
        for n in (5, 300, 70000):
            f = obsws.encode_frame(b"x" * n, mask=b"\0\0\0\0")
            self.assertEqual(f[0], 0x81)
            self.assertTrue(f[1] & 0x80)  # client frames are masked
            self.assertEqual(f[-n:], b"x" * n)

    def test_request_roundtrip_and_events(self):
        server = FakeObsWs()
        events = []
        ws = obsws.ObsWs("127.0.0.1", server.port, "pw", on_event=lambda t, d: events.append(t))
        ws.connect()
        try:
            self.assertEqual(ws.request("StartRecord", {"a": 1}), {"echo": {"a": 1}})
            with self.assertRaises(obsws.ObsWsError):
                ws.request("Fail")
            time.sleep(0.1)
            self.assertIn("RecordStateChanged", events)
        finally:
            ws.close()

    def test_wrong_password(self):
        server = FakeObsWs(password="right")
        ws = obsws.ObsWs("127.0.0.1", server.port, "wrong", timeout=3)
        with self.assertRaises((obsws.ObsWsError, ConnectionError, OSError)):
            ws.connect()


class EngineConfigTests(unittest.TestCase):
    def options(self, **kw):
        o = {"rec_dir": Path("C:/Videos/OBSPlayed/Game"), "encoder": "obs_nvenc_h264_tex", "quality": "high",
             "fps": 60, "size": (2560, 1440), "capture": "game", "audio": "game_mic", "mic_mono": True,
             "replay_seconds": 30, "window_spec": "Title:Cls:game.exe", "ws_port": 4466, "ws_password": "secret"}
        o.update(kw)
        return o

    def test_writes_profile_scene_and_websocket(self):
        with tempfile.TemporaryDirectory() as d:
            engine.write_config(Path(d), self.options())
            root = Path(d) / "obs-studio"
            basic = (root / "basic/profiles/OBSPlayed/basic.ini").read_text()
            self.assertIn("RecEncoder=obs_nvenc_h264_tex", basic)
            self.assertIn("BaseCX=2560", basic)
            self.assertIn("FPSCommon=60", basic)
            self.assertIn("RecFormat2=hybrid_mp4", basic)
            self.assertIn("RecRBTime=30", basic)
            enc = json.loads((root / "basic/profiles/OBSPlayed/recordEncoder.json").read_text())
            self.assertEqual(enc["rate_control"], "CQP")
            ws = json.loads((root / "plugin_config/obs-websocket/config.json").read_text())
            self.assertEqual((ws["server_port"], ws["server_password"], ws["server_enabled"]), (4466, "secret", True))
            user = (root / "user.ini").read_text()
            self.assertIn("PreviewEnabled=false", user)
            self.assertNotIn("SysTrayMinimizeToTray", user)  # it would stop WM_CLOSE from quitting OBS
            scene = json.loads((root / "basic/scenes/OBSPlayed.json").read_text())
            self.assertEqual(scene["AuxAudioDevice1"]["flags"], engine.FORCE_MONO)
            self.assertFalse(scene["AuxAudioDevice1"]["muted"])        # game + mic
            self.assertTrue(scene["DesktopAudioDevice1"]["muted"])
            by_name = {s["name"]: s for s in scene["sources"]}
            self.assertEqual(by_name["Game"]["settings"]["window"], "Title:Cls:game.exe")
            items = [i["name"] for i in by_name["OBSPlayed"]["settings"]["items"]]
            self.assertLess(items.index("Game Window"), items.index("Game"))  # window capture is below game capture

    def test_audio_choices(self):
        self.assertEqual(engine.audio_mutes("game"), {"Desktop Audio": True, "Mic/Aux": True, "Game Audio": False})
        self.assertEqual(engine.audio_mutes("game_mic")["Mic/Aux"], False)
        self.assertEqual(engine.audio_mutes("all"), {"Desktop Audio": False, "Mic/Aux": False, "Game Audio": True})

    def test_screen_capture_and_mono_off(self):
        coll = engine.scene_collection("screen", "", "game", False, (1920, 1080))
        items = {i["name"]: i["visible"] for i in next(s for s in coll["sources"] if s["name"] == "OBSPlayed")["settings"]["items"]}
        self.assertEqual((items["Screen"], items["Game"]), (True, False))
        self.assertEqual(coll["AuxAudioDevice1"]["flags"], 0)

    def test_pick_encoder(self):
        self.assertEqual(engine.pick_encoder(None, ["AMD Radeon(TM) Graphics", "NVIDIA GeForce RTX 5080"]), "obs_nvenc_h264_tex")
        self.assertEqual(engine.pick_encoder(None, ["AMD Radeon RX 7900"]), "h264_texture_amf")
        self.assertEqual(engine.pick_encoder(None, ["Intel(R) Arc(TM) A770"]), "obs_qsv11_v2")
        self.assertEqual(engine.pick_encoder(None, ["Microsoft Basic Display Adapter"]), "obs_x264")

    def test_engine_events(self):
        api = engine.EngineObs(engine.Engine(log=lambda m: None, obs_dir=Path("C:/nope")), lambda: {})
        self.assertEqual(api.handle_event("RecordStateChanged", {"outputState": "OBS_WEBSOCKET_OUTPUT_STARTED", "outputPath": "a.mp4"}), "recording_started")
        self.assertTrue(api.recording)
        self.assertEqual(api.active_path, "a.mp4")
        self.assertEqual(api.handle_event("RecordStateChanged", {"outputState": "OBS_WEBSOCKET_OUTPUT_STOPPED", "outputPath": "a.mp4"}), "recording_stopped")
        self.assertEqual((api.recording, api.active_path, api.last_recording()), (False, None, "a.mp4"))
        self.assertEqual(api.handle_event("ReplayBufferSaved", {"savedReplayPath": "c.mp4"}), "replay_saved")
        self.assertEqual(api.last_replay(), "c.mp4")
        api.recording = True
        self.assertEqual(api.handle_event("EngineClosed", {}), "recording_stopped")  # OBS crashed mid-recording


class ClipStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "videos"
        (self.root / "Valorant").mkdir(parents=True)
        self.rec = self.root / "Valorant" / "2026-11-04 10-00-00.mp4"
        self.rec.write_bytes(b"\0" * 100)
        self.clip = self.root / "Valorant" / "Clip 2026-11-04 10-05-00 - Triple Kill.mp4"
        self.clip.write_bytes(b"\0" * 50)
        (self.root / "Valorant" / "notes.txt").write_text("x")
        self.store = ClipStore(self.root, Path(self.tmp.name) / "data", log=lambda m: None)

    def tearDown(self):
        self.tmp.cleanup()

    def test_list_guesses_kind_game_and_title(self):
        items = {c["name"]: c for c in self.store.list()}
        self.assertEqual(set(items), {self.rec.name, self.clip.name})
        self.assertEqual(items[self.clip.name]["kind"], "clip")
        self.assertEqual(items[self.clip.name]["title"], "Triple Kill")
        self.assertEqual(items[self.rec.name]["kind"], "session")
        self.assertEqual(items[self.rec.name]["game"], "Valorant")

    def test_add_metadata_and_live(self):
        self.store.add({"kind": "session", "path": str(self.rec), "game": "VALORANT",
                        "markers": [{"offset": 3.0, "label": "Kill", "detail": ""}], "highlights": []})
        c = next(c for c in self.store.list(live={str(self.rec)}) if c["name"] == self.rec.name)
        self.assertEqual(c["game"], "VALORANT")
        self.assertEqual(len(c["markers"]), 1)
        self.assertTrue(c["live"])

    def test_duration_is_forgotten_when_file_changes(self):
        cid = clip_id(self.rec.relative_to(self.root).as_posix())
        self.store.list()
        self.store.update(cid, duration=2.0)
        self.assertEqual(next(c for c in self.store.list() if c["id"] == cid)["duration"], 2.0)
        os.utime(self.rec, (time.time() + 60, time.time() + 60))  # still being written
        self.assertIsNone(next(c for c in self.store.list() if c["id"] == cid)["duration"])

    def test_update_rejects_unknown_fields_and_thumbs_must_be_jpeg(self):
        cid = self.store.list()[0]["id"]
        self.store.update(cid, title="Ace", path="C:/evil")
        rel = next(r for r in self.store.meta)
        self.assertNotIn("path", self.store.meta[rel])
        with self.assertRaises(ValueError):
            self.store.save_thumb(cid, b"<svg>")
        self.store.save_thumb(cid, b"\xff\xd8\xff\xe0jpeg")
        self.assertTrue(next(c for c in self.store.list() if c["id"] == cid)["thumb"])

    def test_trim_needs_ffmpeg(self):
        cid = self.store.list()[0]["id"]
        with self.assertRaises(RuntimeError):
            self.store.trim(cid, 0, 10, None)

    def test_outside_root_ignored(self):
        self.store.add({"kind": "clip", "path": str(Path(self.tmp.name) / "elsewhere.mp4")})
        self.assertEqual(self.store.meta, {})


class AppPrefsTests(unittest.TestCase):
    def test_defaults_coercion_and_invalid_saved_values(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "library.json"
            path.write_text(json.dumps({"prefs": {"capture": "scene", "quality": "keep", "fps": 0, "audio": "game_mic"}}))
            lib = AppLibrary(path)
            p = lib.prefs()
            # values from the plugin version that the app doesn't have fall back to the defaults
            self.assertEqual((p["capture"], p["quality"], p["fps"], p["audio"]), ("game", "high", 60, "game_mic"))
            lib.set_pref("mic_mono", "false")
            lib.set_pref("fps", "30")
            lib.set_pref("clip_length", 60)
            self.assertEqual((lib.prefs()["mic_mono"], lib.prefs()["fps"], lib.prefs()["clip_length"]), (False, 30, 60))
            with self.assertRaises(ValueError):
                lib.set_pref("hotkey", "F1")


class TrayHelperTests(unittest.TestCase):
    def test_parse_hotkey(self):
        self.assertEqual(parse_hotkey("F8"), (0, 0x77))
        self.assertEqual(parse_hotkey("Ctrl+F8"), (MOD_CONTROL, 0x77))
        self.assertIsNone(parse_hotkey("off"))

    def test_icon_is_valid_ico(self):
        ico = make_ico((239, 68, 68))
        self.assertEqual(struct.unpack("<HHH", ico[:6]), (0, 1, 1))
        size = struct.unpack("<I", ico[14:18])[0]
        self.assertEqual(len(ico), 22 + size)


if __name__ == "__main__":
    unittest.main()
