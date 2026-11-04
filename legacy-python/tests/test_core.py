import json
import os
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from obsplayed_core import modes, viewer  # noqa: E402
from obsplayed_core.controller import _rename_replay  # noqa: E402
from obsplayed_core.gamedb import GameDB, slim  # noqa: E402
from obsplayed_core.library import Library  # noqa: E402
from obsplayed_core.panel import Panel  # noqa: E402
from obsplayed_core.procwatch import Snapshot  # noqa: E402
from obsplayed_core.controller import Controller, Settings  # noqa: E402
from obsplayed_core.events import EventBus, GameEvent  # noqa: E402
from obsplayed_core.games import KNOWN_GAMES, detect_game, parse_custom_games  # noqa: E402
from obsplayed_core.procwatch import running_processes  # noqa: E402
from obsplayed_core.providers import lol  # noqa: E402
from obsplayed_core.providers.gsi import CS2Tracker, Dota2Tracker, GSIServer  # noqa: E402
from obsplayed_core.session import Marker, Session, find_highlights  # noqa: E402


def tmp_library(default=None):
    lib = Library(Path(tempfile.mkdtemp()) / "library.json")
    if default:
        lib.set_default_mode(default)
    return lib


def m(offset, kind="kill", weight=1.0):
    return Marker(offset, kind, kind.title(), "G", "", weight)


class HighlightTests(unittest.TestCase):
    def test_single_kill_below_threshold_is_dropped(self):
        self.assertEqual(find_highlights([m(30)], min_score=2.0), [])

    def test_cluster_becomes_highlight(self):
        hl = find_highlights([m(30), m(34), m(39), m(200)], min_score=2.0, pre=8, post=4)
        self.assertEqual(len(hl), 1)
        self.assertEqual((hl[0].start, hl[0].end), (22, 43))
        self.assertEqual(hl[0].title, "3 Kills")

    def test_big_event_title_wins(self):
        hl = find_highlights([m(10), m(11, "triple_kill", 2.0), m(12)], min_score=2.0)
        self.assertEqual(hl[0].title, "Triple Kill (2 kills)")

    def test_overlapping_clips_merge(self):
        hl = find_highlights([m(10, "manual", 10), m(25, "manual", 10)], merge_gap=5, pre=8, post=8)
        self.assertEqual(len(hl), 1)
        self.assertEqual((hl[0].start, hl[0].end), (2, 33))

    def test_start_clamped_to_zero(self):
        hl = find_highlights([m(2, "manual", 10)], pre=8)
        self.assertEqual(hl[0].start, 0)


class LeagueTests(unittest.TestCase):
    me = "Faker#KR1"

    def test_kill_death_assist(self):
        self.assertEqual([e.kind for e in lol.translate({"EventName": "ChampionKill", "KillerName": "Faker", "VictimName": "X"}, self.me)], ["kill"])
        self.assertEqual([e.kind for e in lol.translate({"EventName": "ChampionKill", "KillerName": "X", "VictimName": "faker#kr1"}, self.me)], ["death"])
        self.assertEqual([e.kind for e in lol.translate({"EventName": "ChampionKill", "KillerName": "X", "VictimName": "Y", "Assisters": ["Faker"]}, self.me)], ["assist"])
        self.assertEqual(lol.translate({"EventName": "ChampionKill", "KillerName": "X", "VictimName": "Y"}, self.me), [])

    def test_multikill_and_objectives(self):
        self.assertEqual(lol.translate({"EventName": "Multikill", "KillerName": "Faker", "KillStreak": 5}, self.me)[0].kind, "penta_kill")
        ev = lol.translate({"EventName": "BaronKill", "KillerName": "Faker", "Stolen": "True"}, self.me)[0]
        self.assertEqual((ev.kind, ev.detail), ("objective_steal", "Baron Nashor"))
        ev = lol.translate({"EventName": "DragonKill", "KillerName": "Z", "Assisters": ["Faker"], "DragonType": "Elder", "Stolen": "False"}, self.me)[0]
        self.assertEqual((ev.kind, ev.detail), ("objective", "Elder Dragon"))


def cs(kills, deaths=0, mvps=0, rk=0, rhs=0, steamid="1", spectating=None, mapname="de_dust2"):
    return {
        "provider": {"appid": 730, "steamid": steamid},
        "map": {"name": mapname},
        "player": {
            "steamid": spectating or steamid,
            "state": {"round_kills": rk, "round_killhs": rhs},
            "match_stats": {"kills": kills, "deaths": deaths, "mvps": mvps},
        },
        "auth": {"token": "obsplayed"},
    }


class CS2Tests(unittest.TestCase):
    def test_kills_headshots_multikill(self):
        t = CS2Tracker()
        self.assertEqual(t.update(cs(5)), [])  # baseline
        self.assertEqual([e.kind for e in t.update(cs(6, rk=1, rhs=1))], ["headshot"])
        self.assertEqual([e.kind for e in t.update(cs(8, rk=3, rhs=1))], ["kill", "kill", "triple_kill"])
        self.assertEqual([e.kind for e in t.update(cs(8, deaths=1, rk=3, rhs=1))], ["death"])
        # new round resets round counters
        self.assertEqual([e.kind for e in t.update(cs(9, deaths=1, rk=1, rhs=0))], ["kill"])

    def test_ignores_spectated_player(self):
        t = CS2Tracker()
        t.update(cs(5))
        self.assertEqual(t.update(cs(9, spectating="2")), [])
        self.assertEqual(t.update(cs(5)), [])

    def test_map_change_rebaselines(self):
        t = CS2Tracker()
        t.update(cs(20))
        self.assertEqual(t.update(cs(0, mapname="de_mirage")), [])
        self.assertEqual([e.kind for e in t.update(cs(1, mapname="de_mirage", rk=1))], ["kill"])


class DotaTests(unittest.TestCase):
    def test_diff(self):
        t = Dota2Tracker()
        base = {"map": {"matchid": "9"}, "player": {"kills": 0, "deaths": 0, "assists": 0}}
        self.assertEqual(t.update(base), [])
        nxt = {"map": {"matchid": "9"}, "player": {"kills": 2, "deaths": 1, "assists": 1}}
        self.assertEqual([e.kind for e in t.update(nxt)], ["kill", "kill", "death", "assist"])


class GSIServerTests(unittest.TestCase):
    def test_http_roundtrip_and_auth(self):
        bus = EventBus()
        srv = GSIServer(bus, port=0, log=lambda *_: None)
        srv.port = 0
        srv.start()
        try:
            port = srv._server.server_address[1]

            def post(payload):
                req = urllib.request.Request(f"http://127.0.0.1:{port}/", data=json.dumps(payload).encode(), method="POST")
                urllib.request.urlopen(req, timeout=2).read()

            post(cs(1))
            post(cs(2, rk=1))
            bad = cs(3, rk=2)
            bad["auth"]["token"] = "wrong"
            post(bad)
            self.assertEqual([e.kind for e in bus.drain()], ["kill"])
        finally:
            srv.stop()


class FakeObs:
    def __init__(self):
        self.recording = False
        self.chapters = []
        self.calls = []
        self.path = None
        self.replay = False
        self.replay_path = None

    def recording_active(self):
        return self.recording

    def start_recording(self):
        self.calls.append("start")
        self.recording = True

    def stop_recording(self):
        self.calls.append("stop")
        self.recording = False

    def add_chapter(self, name):
        self.chapters.append(name)
        return True

    def replay_active(self):
        return self.replay

    def start_replay(self):
        self.calls.append("replay_start")
        self.replay = True

    def stop_replay(self):
        self.calls.append("replay_stop")
        self.replay = False

    def save_replay(self):
        self.calls.append("replay_save")

    def last_replay(self):
        return self.replay_path

    track_capture = False  # capture calls are only recorded by the tests that check them

    def prepare_capture(self, window_spec, audio):
        if self.track_capture:
            self.calls.append(("capture", window_spec, audio))
        return True

    def update_capture(self, window_spec):
        if self.track_capture:
            self.calls.append(("update", window_spec))

    def restore_capture(self):
        if self.track_capture:
            self.calls.append("restore")

    def apply_quality(self, prefs):
        self.quality_prefs = prefs
        return {"fps": 60}

    def set_scene(self, name):
        return True

    def last_recording(self):
        return self.path


class ControllerTests(unittest.TestCase):
    def test_full_session(self):
        now = [100.0]
        procs = [set()]
        api = FakeObs()
        c = Controller(api, Settings(exit_grace=5, poll_interval=1, cut_clips=False), log=lambda *_: None,
                       process_lister=lambda: procs[0], clock=lambda: now[0], start_providers=False, library=tmp_library("full"))
        c.tick()
        self.assertEqual(api.calls, [])

        procs[0] = {"explorer.exe", "cs2.exe"}
        now[0] = 102
        c.tick()
        self.assertEqual(api.calls, ["start"])
        c.on_recording_started()  # OBS fires this

        c.bus.emit(GameEvent("kill", "Counter-Strike 2", t=110))
        c.bus.emit(GameEvent("kill", "Dota 2", t=111))  # wrong game: ignored
        c.bus.emit(GameEvent("triple_kill", "Counter-Strike 2", t=112))
        now[0] = 113
        c.tick()
        self.assertEqual([mk.kind for mk in c.session.markers], ["kill", "triple_kill"])
        self.assertEqual(c.session.markers[0].offset, 8.0)
        self.assertEqual(api.chapters, ["Kill", "Triple Kill"])

        procs[0] = {"explorer.exe"}
        now[0] = 120
        c.tick()
        self.assertEqual(api.calls, ["start"])  # grace period
        now[0] = 126
        c.tick()
        self.assertEqual(api.calls, ["start", "stop"])

    def test_does_not_stop_recording_it_did_not_start(self):
        now = [0.0]
        procs = [{"cs2.exe"}]
        api = FakeObs()
        api.recording = True
        c = Controller(api, Settings(exit_grace=0, poll_interval=1), log=lambda *_: None,
                       process_lister=lambda: procs[0], clock=lambda: now[0], start_providers=False, library=tmp_library("full"))
        c.tick()
        procs[0] = set()
        for t in (2, 4):
            now[0] = t
            c.tick()
        self.assertEqual(api.calls, [])

    def _finalize(self, chapters_ok, **settings):
        d = tempfile.mkdtemp()
        rec = os.path.join(d, "2026-11-03 20-00-00.mkv")
        Path(rec).write_bytes(b"x" * 100)
        c = Controller(FakeObs(), Settings(cut_clips=False, **settings), log=lambda *_: None,
                       process_lister=set, clock=time.monotonic, start_providers=False, library=tmp_library())
        s = Session(started_mono=0, game="Counter-Strike 2", chapters_ok=chapters_ok)
        for t in (30, 33, 35):
            s.add(GameEvent("kill", "Counter-Strike 2", t=t))
        c._finalize(s, rec, c.settings)
        return Path(d, "2026-11-03 20-00-00.obsplayed.json"), Path(d, "2026-11-03 20-00-00.highlights.html")

    def test_no_session_files_when_chapters_worked(self):
        js, page = self._finalize(chapters_ok=True)
        self.assertFalse(js.exists())
        self.assertFalse(page.exists())

    def test_session_files_written_when_chapters_failed(self):
        js, page = self._finalize(chapters_ok=False)
        self.assertEqual(len(json.loads(js.read_text(encoding="utf-8"))["highlights"]), 1)
        self.assertIn("2026-11-03%2020-00-00.mkv", page.read_text(encoding="utf-8"))

    def test_session_files_when_forced(self):
        js, page = self._finalize(chapters_ok=True, write_session_files=True)
        self.assertTrue(js.exists() and page.exists())

    def test_failed_chapter_marks_session(self):
        api = FakeObs()
        api.add_chapter = lambda name: False  # e.g. plain MKV recording
        c = Controller(api, Settings(), log=lambda *_: None, process_lister=set, start_providers=False, library=tmp_library())
        c.on_recording_started()
        c.mark_manual()
        c.tick()
        self.assertFalse(c.session.chapters_ok)


class ModeTests(unittest.TestCase):
    def make(self, game_modes=None, **settings):
        self.now = [0.0]
        self.procs = [set()]
        self.api = FakeObs()
        self.asked = []
        self.lib = tmp_library()
        for name, mode in (game_modes or {}).items():
            self.lib.set_mode(name, mode)
        s = Settings(poll_interval=1, exit_grace=0, cut_clips=False, **settings)
        self.c = Controller(self.api, s, log=lambda *_: None, process_lister=lambda: self.procs[0],
                            clock=lambda: self.now[0], start_providers=False, library=self.lib,
                            asker=lambda game, cb: self.asked.append((game, cb)))
        return self.c

    def step(self, t):
        self.now[0] = t
        self.c.tick()

    def test_parse_mode_lines(self):
        self.assertEqual(modes.parse_modes("CS2 = full\nDota 2=Highlights\nx = bogus\n# c = off"),
                         {"cs2": "full", "dota 2": "highlights"})

    def test_first_time_prompt_saves_choice_and_starts(self):
        self.make()
        self.procs[0] = {"valorant-win64-shipping.exe"}
        self.step(1)
        self.assertEqual([g for g, _ in self.asked], ["VALORANT"])
        self.assertEqual(self.api.calls, [])  # nothing until answered
        self.step(2)
        self.assertEqual(len(self.asked), 1)  # not asked twice
        self.asked[0][1]("VALORANT", "full")
        self.step(3)
        self.assertEqual(self.api.calls, ["start"])
        self.assertEqual(self.lib.mode_for("VALORANT"), "full")
        # next launch: no prompt
        self.procs[0] = set()
        self.step(4)
        self.step(5)
        self.procs[0] = {"valorant-win64-shipping.exe"}
        self.step(6)
        self.assertEqual(len(self.asked), 1)
        self.assertEqual(self.api.calls, ["start", "stop", "start"])

    def test_closing_prompt_records_nothing_and_asks_again(self):
        self.make()
        self.procs[0] = {"cs2.exe"}
        self.step(1)
        self.asked[0][1]("Counter-Strike 2", None)
        self.step(2)
        self.assertEqual(self.api.calls, [])
        self.assertEqual(self.lib.mode_for("Counter-Strike 2"), "ask")
        self.procs[0] = set()
        self.step(3)
        self.step(4)
        self.procs[0] = {"cs2.exe"}
        self.step(5)
        self.assertEqual(len(self.asked), 2)

    def test_off_mode(self):
        self.make({"Counter-Strike 2": "off"})
        self.procs[0] = {"cs2.exe"}
        self.step(1)
        self.c.bus.emit(GameEvent("ace", "Counter-Strike 2"))
        self.step(20)
        self.assertEqual(self.api.calls, [])
        self.assertEqual(self.asked, [])

    def test_highlights_mode_saves_replay_for_hot_moment(self):
        self.make({"Counter-Strike 2": "highlights"}, replay_delay=5, min_score=2)
        self.procs[0] = {"cs2.exe"}
        self.step(1)
        self.assertEqual(self.api.calls, ["replay_start"])
        self.c.bus.emit(GameEvent("kill", "Counter-Strike 2"))
        self.step(10)
        self.step(30)  # single kill: below the threshold, no clip
        self.c.bus.emit(GameEvent("kill", "Counter-Strike 2"))
        self.step(40)
        self.c.bus.emit(GameEvent("kill", "Counter-Strike 2"))
        self.step(42)
        self.assertNotIn("replay_save", self.api.calls)
        self.step(47)  # 5 s after the second kill
        self.assertEqual(self.api.calls, ["replay_start", "replay_save"])
        self.assertEqual(self.c.replay_titles, ["2 Kills"])
        self.procs[0] = set()
        self.step(50)
        self.step(51)
        self.assertEqual(self.api.calls[-1], "replay_stop")
        self.assertNotIn("start", self.api.calls)

    def test_pending_clip_is_saved_when_game_closes(self):
        self.make({"Counter-Strike 2": "highlights"})
        self.procs[0] = {"cs2.exe"}
        self.step(1)
        self.c.bus.emit(GameEvent("ace", "Counter-Strike 2"))
        self.step(2)
        self.procs[0] = set()
        self.step(3)
        self.step(4)
        self.assertEqual(self.api.calls, ["replay_start", "replay_save", "replay_stop"])

    def test_replay_saved_gets_renamed(self):
        with tempfile.TemporaryDirectory() as d:
            src = Path(d, "Replay 2026-11-03 21-00-00.mkv")
            src.write_bytes(b"x")
            _rename_replay(str(src), "Triple Kill", log=lambda *_: None)
            self.assertTrue(Path(d, "Replay 2026-11-03 21-00-00 - Triple Kill.mkv").exists())


DISCORD_SAMPLE = [
    {"id": "1", "name": "ELDEN RING", "icon_hash": "abc",
     "executables": [{"os": "win32", "name": "eldenring.exe", "is_launcher": False}]},
    {"id": "2", "name": "Some Unity Game", "icon_hash": None,
     "executables": [{"os": "win32", "name": "some unity game/game.exe", "is_launcher": False}]},
    {"id": "3", "name": "Other Game", "icon_hash": None,
     "executables": [{"os": "win32", "name": "game.exe", "is_launcher": False},
                     {"os": "darwin", "name": "other.app", "is_launcher": False}]},
    {"id": "4", "name": "Launcher Only", "executables": [{"os": "win32", "name": "launch.exe", "is_launcher": True}]},
    {"id": "5", "name": "Elden Ring Mod Tool", "executables": [{"os": "win32", "name": "ermt.exe"}]},
]


def sample_db():
    db = GameDB(tempfile.mkdtemp(), log=lambda *_: None, fetch=lambda: DISCORD_SAMPLE)
    db.refresh()
    return db


class GameDBTests(unittest.TestCase):
    def test_slim_drops_launchers_and_other_os(self):
        names = [g["name"] for g in slim(DISCORD_SAMPLE)]
        self.assertNotIn("Launcher Only", names)
        self.assertEqual(slim(DISCORD_SAMPLE)[2]["exes"], ["game.exe"])

    def test_match_plain_and_path_and_generic(self):
        db = sample_db()
        self.assertEqual(db.match({"eldenring.exe"}).name, "ELDEN RING")
        # generic exe names need the folder to match
        self.assertIsNone(db.match({"game.exe"}))
        snap = Snapshot(paths={"game.exe": ["d:/steam/steamapps/common/some unity game/game.exe"]})
        self.assertEqual(db.match(snap).name, "Some Unity Game")
        self.assertIsNone(db.match(Snapshot(paths={"game.exe": ["c:/other/game.exe"]})))

    def test_search_and_cache(self):
        db = sample_db()
        self.assertEqual([g.name for g in db.search("elden")], ["ELDEN RING", "Elden Ring Mod Tool"])
        again = GameDB(db.path.parent, log=lambda *_: None)
        self.assertTrue(again.load_cached())
        self.assertEqual(again.get("1").icon_url, "https://cdn.discordapp.com/app-icons/1/abc.png?size=64")


class ImporterTests(unittest.TestCase):
    def test_vdf(self):
        from obsplayed_core.importers import parse_vdf
        d = parse_vdf('"A"\n{\n  "b"  "1"\n  "C" { "d" "x \\"q\\"" }\n}')
        self.assertEqual(d, {"A": {"b": "1", "C": {"d": 'x "q"'}}})

    def test_steam_import(self):
        from obsplayed_core.importers import steam_games
        root = Path(tempfile.mkdtemp())
        (root / "steamapps").mkdir()
        (root / "steamapps" / "appmanifest_100.acf").write_text('"AppState" { "appid" "100" "name" "ELDEN RING" }')
        (root / "steamapps" / "appmanifest_228980.acf").write_text(
            '"AppState" { "appid" "228980" "name" "Steamworks Common Redistributables" }')
        cfg = root / "userdata" / "42" / "config"
        cfg.mkdir(parents=True)
        cfg.joinpath("localconfig.vdf").write_text(
            '"UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" {'
            ' "100" { "LastPlayed" "1700000000" "Playtime" "90" }'
            ' "730" { "LastPlayed" "1700000500" "Playtime" "6000" }'
            ' "999" { "Playtime" "5" } } } } } }')
        sample = [dict(DISCORD_SAMPLE[0], third_party_skus=[{"distributor": "steam", "id": "100"}]),
                  {"id": "9", "name": "Counter-Strike 2", "executables": [{"os": "win32", "name": "win64/cs2.exe"}],
                   "third_party_skus": [{"distributor": "steam", "id": "730"}]}]
        db = GameDB(tempfile.mkdtemp(), log=lambda *_: None, fetch=lambda: sample)
        db.refresh()
        found = {g["name"]: g for g in steam_games(db, [root])}
        self.assertEqual(set(found), {"ELDEN RING", "Counter-Strike 2"})  # redist skipped, unknown 999 skipped
        self.assertEqual((found["ELDEN RING"]["playtime"], found["ELDEN RING"]["installed"]), (5400, True))
        self.assertEqual((found["Counter-Strike 2"]["installed"], found["Counter-Strike 2"]["last_played"]), (False, 1700000500))
        lib = tmp_library()
        lib.set_mode("ELDEN RING", "off")
        self.assertEqual(lib.merge_imported(list(found.values())), 1)
        self.assertEqual(lib.mode_for("ELDEN RING"), "off")  # user's choice kept


class LibraryTests(unittest.TestCase):
    def test_history_modes_and_persistence(self):
        path = Path(tempfile.mkdtemp()) / "library.json"
        lib = Library(path)
        self.assertEqual(lib.mode_for("X"), "ask")
        lib.game_started("X", exes=["x.exe"])
        lib.game_ended("X", 125)
        lib.set_mode("X", "highlights")
        lib.set_default_mode("off")
        again = Library(path)
        g = again.games()[0]
        self.assertEqual((g["name"], g["sessions"], g["playtime"], g["mode"]), ("X", 1, 125, "highlights"))
        self.assertEqual(again.mode_for("Unknown"), "off")
        self.assertEqual(again.token, lib.token)
        again.set_mode("X", "ask")
        self.assertEqual(again.mode_for("X"), "off")  # "ask" entries use the default

    def test_import_old_text_settings(self):
        lib = tmp_library()
        lib.import_text_settings("Counter-Strike 2 = highlights", "mygame.exe = My Game")
        self.assertEqual(lib.mode_for("Counter-Strike 2"), "highlights")
        self.assertEqual(lib.custom_exes(), [("mygame.exe", "My Game")])


class DetectionTests(unittest.TestCase):
    def make(self):
        self.procs = [set()]
        self.now = [0.0]
        self.asked = []
        self.lib = tmp_library()
        self.c = Controller(FakeObs(), Settings(poll_interval=1, exit_grace=0), log=lambda *_: None,
                            process_lister=lambda: self.procs[0], clock=lambda: self.now[0],
                            start_providers=False, library=self.lib, gamedb=sample_db(),
                            asker=lambda g, cb: self.asked.append(g) or (lambda: self.closed.append(g)))
        self.closed = []

    def step(self, t):
        self.now[0] = t
        self.c.tick()

    def test_database_game_is_detected_and_added_to_history(self):
        self.make()
        self.procs[0] = {"eldenring.exe"}
        self.step(1)
        self.assertEqual(self.asked, ["ELDEN RING"])
        g = self.lib.games()[0]
        self.assertEqual((g["name"], g["source"], g["discord_id"], g["sessions"]), ("ELDEN RING", "discord", "1", 1))

    def test_custom_game_and_builtin_priority(self):
        self.make()
        self.lib.add("My Game", exes=["mygame.exe"])
        self.procs[0] = {"mygame.exe"}
        self.step(1)
        self.assertEqual(self.c.current.name, "My Game")
        self.procs[0] = {"cs2.exe", "eldenring.exe"}
        self.step(2)
        self.assertEqual(self.c.current.name, "Counter-Strike 2")

    def test_answer_from_panel_closes_popup(self):
        self.make()
        self.procs[0] = {"eldenring.exe"}
        self.step(1)
        self.c.answer("ELDEN RING", "full")
        self.step(2)
        self.assertEqual(self.closed, ["ELDEN RING"])
        self.assertEqual(self.c.mode, "full")
        self.assertEqual(self.lib.mode_for("ELDEN RING"), "full")


class CaptureTests(unittest.TestCase):
    def test_game_scene_audio_and_restore(self):
        from unittest import mock
        import obsplayed_core.controller as ctl
        procs = [Snapshot(pids={"cs2.exe": [111]})]
        now = [0.0]
        api = FakeObs()
        api.track_capture = True
        lib = tmp_library("full")
        lib.set_pref("audio", "game_mic")
        c = Controller(api, Settings(poll_interval=1, exit_grace=0, cut_clips=False), log=lambda *_: None,
                       process_lister=lambda: procs[0], clock=lambda: now[0], start_providers=False, library=lib)
        windows = [None]
        with mock.patch.object(ctl, "main_window", lambda pids: windows[0] if pids == [111] else None):
            c.tick()
            self.assertEqual(api.quality_prefs["quality"], "high")  # quality applied on start
            self.assertEqual(api.calls, ["start"])
            c.on_output_starting()  # OBS: "recording starting" (UI thread) -> switch scene now
            self.assertEqual(api.calls, ["start", ("capture", "::cs2.exe", "game_mic")])
            windows[0] = ("Counter-Strike 2", "SDL_app")
            now[0] = 2
            c.tick()
            self.assertEqual(api.calls[-1], ("update", "Counter-Strike 2:SDL_app:cs2.exe"))
            now[0] = 4
            c.tick()
            self.assertEqual(sum(1 for x in api.calls if x[0] == "update"), 1)  # only once
            procs[0] = Snapshot()
            for t in (6, 8):
                now[0] = t
                c.tick()
        self.assertEqual(api.calls[-1], "stop")
        c.on_output_stopped()  # OBS: "recording stopped" (UI thread) -> put the scene back
        self.assertEqual(api.calls[-1], "restore")

    def test_my_scene_mode_leaves_scene_alone(self):
        procs = [Snapshot(pids={"cs2.exe": [1]})]
        api = FakeObs()
        api.track_capture = True
        lib = tmp_library("full")
        lib.set_pref("capture", "scene")
        c = Controller(api, Settings(poll_interval=1), log=lambda *_: None, process_lister=lambda: procs[0],
                       start_providers=False, library=lib)
        c.tick()
        self.assertEqual(api.calls, ["start"])

    def test_prefs_validation(self):
        lib = tmp_library()
        self.assertEqual(lib.prefs(), {"capture": "game", "audio": "game", "quality": "high", "fps": 0})
        lib.set_pref("fps", "60")
        self.assertEqual(lib.prefs()["fps"], 60)
        with self.assertRaises(ValueError):
            lib.set_pref("audio", "loud")
        with self.assertRaises(KeyError):
            lib.set_pref("bogus", 1)

    def test_window_spec_escaping(self):
        from obsplayed_core.procwatch import obs_window_spec
        self.assertEqual(obs_window_spec("a.exe", ("T: #1", "C")), "T#3A #221:C:a.exe")


class ScanSkipTests(unittest.TestCase):
    def test_scan_skipped_when_launchers_unchanged(self):
        from unittest import mock
        import obsplayed_core.panel as panel_mod
        lib = tmp_library()
        logs = []
        p = Panel(lib, sample_db(), lambda: None, port=0, log=logs.append)
        calls = []
        with mock.patch.object(panel_mod.importers, "signature", lambda: "sig-1"), \
             mock.patch.object(panel_mod.importers, "scan", lambda db: calls.append(1) or []):
            for force in (False, False, True):
                p.scan_async(force=force)
                for _ in range(100):
                    if not p.scanning:
                        break
                    time.sleep(0.01)
        self.assertEqual(len(calls), 2)  # first scan, skipped, forced
        self.assertIn("[scan] nothing changed since the last scan", logs)


class QualityTests(unittest.TestCase):
    def test_presets(self):
        from obsplayed_core import quality
        self.assertEqual(quality.pick_encoder(["obs_x264", "obs_nvenc_h264_tex"], "obs_nvenc_h264_tex"), "obs_nvenc_h264_tex")
        self.assertEqual(quality.pick_encoder(["obs_x264", "h264_texture_amf"], "obs_x264"), "h264_texture_amf")
        self.assertEqual(quality.pick_encoder(["obs_x264"]), "obs_x264")
        nv = quality.encoder_settings("obs_nvenc_h264_tex", "high")
        self.assertEqual((nv["rate_control"], nv["cqp"]), ("CQP", 20))
        self.assertEqual(quality.encoder_settings("obs_x264", "max")["crf"], 16)
        self.assertLess(quality.encoder_settings("h264_texture_amf", "max")["cqp"],
                        quality.encoder_settings("h264_texture_amf", "small")["cqp"])


class PanelTests(unittest.TestCase):
    def setUp(self):
        self.lib = tmp_library()
        self.c = Controller(FakeObs(), Settings(), log=lambda *_: None, process_lister=set,
                            start_providers=False, library=self.lib, gamedb=sample_db())
        self.panel = Panel(self.lib, self.c.gamedb, lambda: self.c, port=0, log=lambda *_: None)
        self.panel.start()
        self.panel.port = self.panel._server.server_address[1]

    def tearDown(self):
        self.panel.stop()

    def call(self, path, body=None, token=True, host=None):
        headers = {"Host": host or f"127.0.0.1:{self.panel.port}"}
        if token:
            headers["X-OBSPlayed-Token"] = self.lib.token
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(f"http://127.0.0.1:{self.panel.port}{path}", data=data, headers=headers,
                                     method="POST" if data else "GET")
        try:
            with urllib.request.urlopen(req, timeout=3) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def test_security(self):
        self.assertEqual(self.call("/api/state", token=False)[0], 403)
        self.assertEqual(self.call("/api/state", host="evil.example:80")[0], 403)
        self.assertEqual(self.call("/?t=wrong", token=False)[0], 403)
        status, page = self.call(f"/?t={self.lib.token}", token=False)
        self.assertEqual(status, 200)
        self.assertIn(self.lib.token.encode(), page)

    def test_add_set_mode_and_state(self):
        self.assertEqual(self.call("/api/add", {"id": "1"})[0], 200)
        self.assertEqual(self.call("/api/add", {"exe": "Foo", "name": "Foo Game"})[0], 200)
        self.assertEqual(self.call("/api/mode", {"name": "ELDEN RING", "mode": "highlights"})[0], 200)
        self.assertEqual(self.call("/api/mode", {"name": "ELDEN RING", "mode": "bogus"})[0], 400)
        self.assertEqual(self.call("/api/default", {"mode": "full"})[0], 200)
        state = json.loads(self.call("/api/state")[1])
        games = {g["name"]: g for g in state["games"]}
        self.assertEqual(games["ELDEN RING"]["mode"], "highlights")
        self.assertTrue(games["ELDEN RING"]["icon"].startswith("https://cdn.discordapp.com/"))
        self.assertEqual(games["Foo Game"]["exes"], ["foo.exe"])
        self.assertEqual(state["default_mode"], "full")
        found = json.loads(self.call("/api/search?q=eld")[1])
        self.assertEqual([(r["name"], r["added"]) for r in found], [("ELDEN RING", True), ("Elden Ring Mod Tool", False)])


class MiscTests(unittest.TestCase):
    def test_custom_games(self):
        games = parse_custom_games("# comment\nMyGame.exe = My Game\nother\n")
        self.assertEqual([(g.name, g.exes) for g in games], [("My Game", ("mygame.exe",)), ("other", ("other.exe",))])
        self.assertEqual(detect_game({"other.exe"}, games + list(KNOWN_GAMES)).name, "other")

    def test_process_list_contains_python(self):
        if sys.platform == "win32":
            self.assertTrue(any(p.startswith("python") or p.startswith("py") for p in running_processes()))

    def test_viewer_escapes_script(self):
        with tempfile.TemporaryDirectory() as d:
            out = viewer.write_viewer(os.path.join(d, "a.mkv"), {"game": "<b>", "markers": [{"detail": "</script>"}], "highlights": []})
            text = out.read_text(encoding="utf-8")
            self.assertNotIn("</script>\"", text)
            self.assertIn("&lt;b&gt;", text)


if __name__ == "__main__":
    unittest.main()
