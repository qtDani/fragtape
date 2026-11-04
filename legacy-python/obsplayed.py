"""OBSPlayed - Outplayed-style auto recording and highlights for OBS Studio.

Load this file in OBS via Tools > Scripts. The ``obsplayed_core`` folder must
sit next to it. Games and their modes are managed in the OBSPlayed panel
(a browser dock; see the README).
"""
import json
import os
import subprocess
import sys
import webbrowser
from pathlib import Path

import obspython as obs

_HERE = os.path.dirname(os.path.abspath(__file__))
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

# Make "Reload Scripts" pick up code changes in the package too.
for _name in [n for n in sys.modules if n == "obsplayed_core" or n.startswith("obsplayed_core.")]:
    del sys.modules[_name]

from obsplayed_core import __version__, gsi_install, quality  # noqa: E402
from obsplayed_core.controller import Controller, Settings  # noqa: E402
from obsplayed_core.gamedb import GameDB  # noqa: E402
from obsplayed_core.library import Library, default_dir  # noqa: E402
from obsplayed_core.panel import Panel  # noqa: E402

HOTKEY_NAME = "obsplayed.mark_highlight"
TICK_MS = 500
DEFAULT_PANEL_PORT = 3132

controller = None
library = None
gamedb = None
panel = None
hotkey_id = obs.OBS_INVALID_HOTKEY_ID
current_settings = Settings()


def log(msg):
    obs.blog(obs.LOG_INFO, f"[OBSPlayed] {msg}")


class ObsFrontend:
    def recording_active(self):
        return obs.obs_frontend_recording_active()

    def start_recording(self):
        obs.obs_frontend_recording_start()

    def stop_recording(self):
        obs.obs_frontend_recording_stop()

    def add_chapter(self, name):
        # Only works with the Hybrid MP4 / MOV recording formats; harmless otherwise.
        try:
            return bool(obs.obs_frontend_recording_add_chapter(name))
        except Exception:
            return False

    def replay_active(self):
        return obs.obs_frontend_replay_buffer_active()

    def start_replay(self):
        obs.obs_frontend_replay_buffer_start()

    def stop_replay(self):
        obs.obs_frontend_replay_buffer_stop()

    def save_replay(self):
        obs.obs_frontend_replay_buffer_save()

    def last_replay(self):
        return obs.obs_frontend_get_last_replay() or None

    def set_scene(self, name):
        source = obs.obs_get_source_by_name(name)
        if not source:
            return False
        try:
            obs.obs_frontend_set_current_scene(source)
            return True
        finally:
            obs.obs_source_release(source)

    def last_recording(self):
        return obs.obs_frontend_get_last_recording() or None

    # ---- capture: OBSPlayed's own game scene ------------------------------
    _restore = None  # what to put back when the game closes

    def prepare_capture(self, window_spec, audio):
        if obs.obs_frontend_streaming_active():
            log("streaming: leaving your scene and audio untouched")
            return False
        global _created_scene
        scene_src = obs.obs_get_source_by_name(GAME_SCENE)
        if scene_src is None:
            # OBS adds new scenes to its list a moment later; hold our reference
            # until the script unloads so the scene can't vanish in between.
            _created_scene = obs.obs_scene_create(GAME_SCENE)
            scene_src = obs.obs_source_get_ref(obs.obs_scene_get_source(_created_scene))
        scene = obs.obs_scene_from_source(scene_src)
        try:
            _put_source(scene, "game_capture", GAME_SOURCE, {
                "capture_mode": "window", "window": window_spec, "priority": 2,  # 2 = match the .exe
                "capture_cursor": False, "allow_transparency": False, "anti_cheat_hook": True,
            }, fit=True, visible=True)
            _put_source(scene, "wasapi_process_output_capture", AUDIO_SOURCE,
                        {"window": window_spec, "priority": 2}, fit=False, visible=audio != "all")

            current = obs.obs_frontend_get_current_scene()
            previous = obs.obs_source_get_name(current) if current else None
            if current:
                obs.obs_source_release(current)
            changed = []
            # Global Desktop Audio (1, 2) and Mic/Aux (3-5) go into every recording.
            # Mute what the chosen audio mode leaves out, unmute what it needs,
            # and remember the original state to put back after the game.
            for channel in (1, 2, 3, 4, 5):
                want_muted = (channel <= 2 and audio != "all") or (channel >= 3 and audio == "game")
                src = obs.obs_get_output_source(channel)
                if src:
                    was_muted = obs.obs_source_muted(src)
                    if was_muted != want_muted:
                        obs.obs_source_set_muted(src, want_muted)
                        changed.append((channel, was_muted))
                        log(f"{'muted' if want_muted else 'unmuted'} '{obs.obs_source_get_name(src)}' while you play")
                    obs.obs_source_release(src)
            if previous != GAME_SCENE:
                obs.obs_frontend_set_current_scene(scene_src)
            ObsFrontend._restore = {"scene": previous if previous != GAME_SCENE else None, "audio": changed}
            return True
        finally:
            obs.obs_source_release(scene_src)

    def update_capture(self, window_spec):
        for name in (GAME_SOURCE, AUDIO_SOURCE):
            src = obs.obs_get_source_by_name(name)
            if src:
                data = obs.obs_data_create()
                obs.obs_data_set_string(data, "window", window_spec)
                obs.obs_source_update(src, data)
                obs.obs_data_release(data)
                obs.obs_source_release(src)

    def restore_capture(self):
        r, ObsFrontend._restore = ObsFrontend._restore, None
        if not r:
            return
        for channel, was_muted in r["audio"]:
            src = obs.obs_get_output_source(channel)
            if src:
                obs.obs_source_set_muted(src, was_muted)
                log(f"{'muted' if was_muted else 'unmuted'} '{obs.obs_source_get_name(src)}' again (as before the game)")
                obs.obs_source_release(src)
        current = obs.obs_frontend_get_current_scene()
        on_game_scene = current is not None and obs.obs_source_get_name(current) == GAME_SCENE
        if current:
            obs.obs_source_release(current)
        if r["scene"] and on_game_scene:  # don't undo a scene change the user made
            src = obs.obs_get_source_by_name(r["scene"])
            if src:
                obs.obs_frontend_set_current_scene(src)
                obs.obs_source_release(src)

    # ---- recording quality ------------------------------------------------
    def apply_quality(self, prefs):
        cfg = obs.obs_frontend_get_profile_config()
        mode = obs.config_get_string(cfg, "Output", "Mode") or "Simple"
        preset = prefs.get("quality", "keep")
        changed = False
        encoder = None
        if preset != "keep":
            if mode == "Advanced":
                encoder = obs.config_get_string(cfg, "AdvOut", "RecEncoder") or "none"
                if encoder == "none":  # recording shares the streaming encoder: give it its own
                    available = [e for c in quality.CANDIDATES.values() for e in c if obs.obs_encoder_get_display_name(e)]
                    encoder = quality.pick_encoder(available, obs.config_get_string(cfg, "AdvOut", "Encoder") or "")
                    obs.config_set_string(cfg, "AdvOut", "RecEncoder", encoder)
                    changed = True
                path = _profile_dir() / "recordEncoder.json"
                path.write_text(json.dumps(quality.encoder_settings(encoder, preset)), encoding="utf-8")
            elif obs.config_get_string(cfg, "SimpleOutput", "RecQuality") != quality.SIMPLE_QUALITY[preset]:
                obs.config_set_string(cfg, "SimpleOutput", "RecQuality", quality.SIMPLE_QUALITY[preset])
                changed = True
        fps = int(prefs.get("fps") or 0)
        ovi = obs.obs_video_info()
        obs.obs_get_video_info(ovi)
        running_fps = round(ovi.fps_num / max(1, ovi.fps_den))
        if fps and running_fps != fps:
            obs.config_set_int(cfg, "Video", "FPSType", 0)
            obs.config_set_string(cfg, "Video", "FPSCommon", str(fps))
            changed = True
        if changed:
            obs.config_save(cfg)
        return {
            "mode": mode,
            "encoder": obs.obs_encoder_get_display_name(encoder) if encoder and encoder != "none" else None,
            "resolution": f"{ovi.output_width}x{ovi.output_height}",
            "fps": running_fps,
            "restart_required": _restart_required(cfg, mode, fps, running_fps),
        }


GAME_SCENE = "OBSPlayed"
GAME_SOURCE = "OBSPlayed Game Capture"
AUDIO_SOURCE = "OBSPlayed Game Audio"
_created_scene = None


def _put_source(scene, source_id, name, settings, fit, visible):
    """Create or update a source in the OBSPlayed scene."""
    data = obs.obs_data_create_from_json(json.dumps(settings))
    src = obs.obs_get_source_by_name(name)
    if src is None:
        src = obs.obs_source_create(source_id, name, data, None)
    else:
        obs.obs_source_update(src, data)
    obs.obs_data_release(data)
    if src is None:
        log(f"could not create {source_id}")
        return
    item = obs.obs_scene_find_source(scene, name) or obs.obs_scene_add(scene, src)
    if item:
        obs.obs_sceneitem_set_visible(item, visible)
        if fit:  # fill the canvas, keeping the aspect ratio
            ovi = obs.obs_video_info()
            obs.obs_get_video_info(ovi)
            bounds, pos = obs.vec2(), obs.vec2()
            bounds.x, bounds.y = ovi.base_width, ovi.base_height
            pos.x, pos.y = 0, 0
            obs.obs_sceneitem_set_bounds_type(item, obs.OBS_BOUNDS_SCALE_INNER)
            obs.obs_sceneitem_set_bounds_alignment(item, 0)
            obs.obs_sceneitem_set_bounds(item, bounds)
            obs.obs_sceneitem_set_pos(item, pos)
    obs.obs_source_release(src)


def _profile_dir():
    cfg = obs.obs_frontend_get_user_config() if hasattr(obs, "obs_frontend_get_user_config") else None
    cfg = cfg or obs.obs_frontend_get_global_config()
    folder = obs.config_get_string(cfg, "Basic", "ProfileDir") or obs.obs_frontend_get_current_profile()
    return Path(os.environ["APPDATA"]) / "obs-studio" / "basic" / "profiles" / folder


def _restart_required(cfg, mode, fps, running_fps):
    """True when the saved settings differ from what OBS is running with."""
    if fps and fps != running_fps:
        return True
    if mode == "Advanced" and (obs.config_get_string(cfg, "AdvOut", "RecEncoder") or "none") != "none":
        # OBS only creates a separate recording encoder when it builds its outputs
        # (at startup or after Settings > Apply), so until then it's missing.
        enc = obs.obs_get_encoder_by_name("advanced_video_recording")
        if enc:
            obs.obs_encoder_release(enc)
            return False
        return True
    return False


# ---- OBS script API ---------------------------------------------------------

def script_description():
    url = panel.url if panel else ""
    return (
        f"<h2>OBSPlayed {__version__}</h2>"
        "<p>Records your games automatically and catches kills and hot moments, like Outplayed.</p>"
        "<p><b>Your games and what to record for each one are in the OBSPlayed panel</b> "
        "(Docks menu). To add the panel yourself: Docks &gt; Custom Browser Docks, "
        f"name <i>OBSPlayed</i>, URL:<br><code>{url}</code></p>"
    )


def script_defaults(settings):
    d = Settings()
    obs.obs_data_set_default_int(settings, "panel_port", DEFAULT_PANEL_PORT)
    obs.obs_data_set_default_bool(settings, "stop_on_exit", d.stop_on_exit)
    obs.obs_data_set_default_int(settings, "exit_grace", int(d.exit_grace))
    obs.obs_data_set_default_string(settings, "game_scene", d.game_scene)
    obs.obs_data_set_default_bool(settings, "lol_enabled", d.lol_enabled)
    obs.obs_data_set_default_bool(settings, "gsi_enabled", d.gsi_enabled)
    obs.obs_data_set_default_int(settings, "gsi_port", d.gsi_port)
    obs.obs_data_set_default_string(settings, "gsi_token", d.gsi_token)
    obs.obs_data_set_default_bool(settings, "add_chapters", d.add_chapters)
    obs.obs_data_set_default_bool(settings, "save_replay_on_highlight", d.save_replay_on_highlight)
    obs.obs_data_set_default_double(settings, "replay_delay", d.replay_delay)
    obs.obs_data_set_default_double(settings, "replay_max_wait", d.replay_max_wait)
    obs.obs_data_set_default_double(settings, "min_score", d.min_score)
    obs.obs_data_set_default_double(settings, "merge_gap", d.merge_gap)
    obs.obs_data_set_default_double(settings, "pre_roll", d.pre_roll)
    obs.obs_data_set_default_double(settings, "post_roll", d.post_roll)
    obs.obs_data_set_default_bool(settings, "write_session_files", d.write_session_files)
    obs.obs_data_set_default_bool(settings, "cut_clips", d.cut_clips)
    obs.obs_data_set_default_string(settings, "ffmpeg_path", d.ffmpeg_path)


def _group(props, name, title):
    sub = obs.obs_properties_create()
    obs.obs_properties_add_group(props, name, title, obs.OBS_GROUP_NORMAL, sub)
    return sub


def script_properties():
    props = obs.obs_properties_create()

    pan = _group(props, "grp_panel", "Games panel")
    obs.obs_properties_add_button(pan, "open_panel", "Open the games panel in a browser", on_open_panel)
    obs.obs_properties_add_button(pan, "copy_panel", "Copy panel URL (for Docks > Custom Browser Docks)", on_copy_panel)
    obs.obs_properties_add_int(pan, "panel_port", "Panel port", 1024, 65535, 1)

    rec = _group(props, "grp_rec", "Recording")
    obs.obs_properties_add_bool(rec, "stop_on_exit", "Stop recording when the game closes")
    obs.obs_properties_add_int(rec, "exit_grace", "Wait before stopping (seconds)", 0, 300, 1)
    scenes = obs.obs_properties_add_list(
        rec, "game_scene", "Switch to scene on game start", obs.OBS_COMBO_TYPE_EDITABLE, obs.OBS_COMBO_FORMAT_STRING
    )
    obs.obs_property_list_add_string(scenes, "(don't switch)", "")
    for name in obs.obs_frontend_get_scene_names() or []:
        obs.obs_property_list_add_string(scenes, name, name)

    det = _group(props, "grp_det", "Highlight detection")
    obs.obs_properties_add_bool(det, "lol_enabled", "League of Legends (Live Client API)")
    obs.obs_properties_add_bool(det, "gsi_enabled", "CS2 / Dota 2 (Game State Integration)")
    obs.obs_properties_add_int(det, "gsi_port", "GSI port", 1024, 65535, 1)
    obs.obs_properties_add_text(det, "gsi_token", "GSI auth token", obs.OBS_TEXT_DEFAULT)
    obs.obs_properties_add_button(det, "install_gsi", "Install CS2 / Dota 2 GSI config", on_install_gsi)
    obs.obs_properties_add_bool(det, "add_chapters", "Add chapter markers (Hybrid MP4 / MOV recordings)")
    obs.obs_properties_add_bool(det, "save_replay_on_highlight", "Record mode: also save a replay clip on each highlight")
    obs.obs_properties_add_float(det, "replay_delay", "Clips: seconds to wait after the last event", 0, 30, 0.5)
    obs.obs_properties_add_float(det, "replay_max_wait", "Clips: save at most this long after a moment starts", 1, 120, 1)
    obs.obs_properties_add_button(det, "test_mark", "Test: mark a highlight now", on_test_mark)

    out = _group(props, "grp_out", "Highlights")
    obs.obs_properties_add_float(out, "min_score", "Sensitivity (lower = more clips)", 0.5, 20, 0.5)
    obs.obs_properties_add_float(out, "merge_gap", "Merge events closer than (seconds)", 1, 60, 1)
    obs.obs_properties_add_float(out, "pre_roll", "Clip seconds before", 0, 60, 1)
    obs.obs_properties_add_float(out, "post_roll", "Clip seconds after", 0, 60, 1)
    obs.obs_properties_add_bool(out, "write_session_files", "Also write viewer .html + .json (always written if chapters fail)")
    obs.obs_properties_add_bool(out, "cut_clips", "Record mode: export highlight clips with ffmpeg")
    obs.obs_properties_add_path(out, "ffmpeg_path", "ffmpeg.exe (blank = auto-detect)", obs.OBS_PATH_FILE, "ffmpeg (ffmpeg.exe)", None)
    return props


def _read_settings(settings):
    return Settings(
        stop_on_exit=obs.obs_data_get_bool(settings, "stop_on_exit"),
        exit_grace=float(obs.obs_data_get_int(settings, "exit_grace")),
        game_scene=obs.obs_data_get_string(settings, "game_scene"),
        lol_enabled=obs.obs_data_get_bool(settings, "lol_enabled"),
        gsi_enabled=obs.obs_data_get_bool(settings, "gsi_enabled"),
        gsi_port=obs.obs_data_get_int(settings, "gsi_port"),
        gsi_token=obs.obs_data_get_string(settings, "gsi_token"),
        add_chapters=obs.obs_data_get_bool(settings, "add_chapters"),
        save_replay_on_highlight=obs.obs_data_get_bool(settings, "save_replay_on_highlight"),
        replay_delay=obs.obs_data_get_double(settings, "replay_delay"),
        replay_max_wait=obs.obs_data_get_double(settings, "replay_max_wait"),
        min_score=obs.obs_data_get_double(settings, "min_score"),
        merge_gap=obs.obs_data_get_double(settings, "merge_gap"),
        pre_roll=obs.obs_data_get_double(settings, "pre_roll"),
        post_roll=obs.obs_data_get_double(settings, "post_roll"),
        write_session_files=obs.obs_data_get_bool(settings, "write_session_files"),
        cut_clips=obs.obs_data_get_bool(settings, "cut_clips"),
        ffmpeg_path=obs.obs_data_get_string(settings, "ffmpeg_path"),
    )


def script_update(settings):
    global current_settings, panel
    current_settings = _read_settings(settings)
    if controller:
        controller.apply_settings(current_settings)
    port = obs.obs_data_get_int(settings, "panel_port") or DEFAULT_PANEL_PORT
    if panel and panel.port != port:
        panel.stop()
        panel = Panel(library, gamedb, lambda: controller, port, log)
        panel.start()


def _migrate_old_settings(settings):
    # v0.1 kept games in two text boxes; move them into the library.
    modes_text = obs.obs_data_get_string(settings, "game_modes")
    custom = obs.obs_data_get_string(settings, "custom_games")
    if modes_text or custom:
        n = library.import_text_settings(modes_text, custom)
        log(f"moved {n} games from the old settings into the games panel")
        obs.obs_data_erase(settings, "game_modes")
        obs.obs_data_erase(settings, "custom_games")
    default = obs.obs_data_get_string(settings, "default_mode")
    if default:
        library.set_default_mode(default)
        obs.obs_data_erase(settings, "default_mode")


def script_load(settings):
    global controller, hotkey_id, library, gamedb, panel
    library = Library()
    _migrate_old_settings(settings)
    gamedb = GameDB(default_dir(), log)
    controller = Controller(ObsFrontend(), _read_settings(settings), log, library=library, gamedb=gamedb)
    panel = Panel(library, gamedb, lambda: controller, obs.obs_data_get_int(settings, "panel_port") or DEFAULT_PANEL_PORT, log)
    panel.start()
    # Once the game list is ready, look for new Steam / Riot / Epic games. This is
    # skipped almost for free when the launchers' files haven't changed.
    p = panel
    gamedb.start(on_ready=lambda: p.scan_async())
    obs.obs_frontend_add_event_callback(on_frontend_event)
    obs.timer_add(on_tick, TICK_MS)

    hotkey_id = obs.obs_hotkey_register_frontend(HOTKEY_NAME, "OBSPlayed: Mark highlight", on_hotkey)
    arr = obs.obs_data_get_array(settings, HOTKEY_NAME)
    obs.obs_hotkey_load(hotkey_id, arr)
    obs.obs_data_array_release(arr)
    log(f"loaded v{__version__}")


def script_save(settings):
    if hotkey_id != obs.OBS_INVALID_HOTKEY_ID:
        arr = obs.obs_hotkey_save(hotkey_id)
        obs.obs_data_set_array(settings, HOTKEY_NAME, arr)
        obs.obs_data_array_release(arr)


def script_unload():
    global controller, panel
    obs.timer_remove(on_tick)
    obs.obs_frontend_remove_event_callback(on_frontend_event)
    if panel:
        panel.stop()
        panel = None
    if controller:
        controller.shutdown()
        controller = None
    global _created_scene
    if _created_scene is not None:
        obs.obs_scene_release(_created_scene)  # OBS's scene list holds its own reference
        _created_scene = None


# ---- callbacks -------------------------------------------------------------

def on_tick():
    if controller:
        try:
            controller.tick()
        except Exception as exc:
            log(f"tick error: {exc!r}")


def on_frontend_event(event):
    if not controller:
        return
    # These callbacks run on OBS's UI thread, the only safe place to switch scenes.
    if event in (obs.OBS_FRONTEND_EVENT_RECORDING_STARTING, obs.OBS_FRONTEND_EVENT_REPLAY_BUFFER_STARTING):
        controller.on_output_starting()
    elif event == obs.OBS_FRONTEND_EVENT_RECORDING_STARTED:
        controller.on_recording_started()
    elif event == obs.OBS_FRONTEND_EVENT_RECORDING_STOPPED:
        controller.on_recording_stopped()
        controller.on_output_stopped()
    elif event == obs.OBS_FRONTEND_EVENT_REPLAY_BUFFER_STOPPED:
        controller.on_output_stopped()
    elif event == obs.OBS_FRONTEND_EVENT_REPLAY_BUFFER_SAVED:
        controller.on_replay_saved()
    elif event == obs.OBS_FRONTEND_EVENT_EXIT:
        if panel:
            panel.stop()
        controller.shutdown()


def on_hotkey(pressed):
    if pressed and controller:
        controller.mark_manual()


def on_test_mark(props, prop):
    if controller:
        controller.mark_manual()
    return False


def on_open_panel(props, prop):
    if panel:
        webbrowser.open(panel.url)
    return False


def on_copy_panel(props, prop):
    if panel:
        subprocess.run("clip", input=panel.url, text=True, creationflags=0x08000000)
        log(f"panel URL copied: {panel.url}")
    return False


def on_install_gsi(props, prop):
    try:
        for line in gsi_install.install(current_settings.gsi_port, current_settings.gsi_token):
            log(line)
        log("GSI configs installed. Restart the game. Dota 2 also needs the launch option -gamestateintegration")
    except Exception as exc:
        log(f"GSI install failed: {exc!r}")
    return False
