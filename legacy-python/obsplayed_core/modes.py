"""Per-game recording modes and the first-time "what should I record?" prompt."""
from __future__ import annotations

import os
import subprocess
import sys
import threading
from typing import Callable

FULL = "full"              # record the whole session (kills become chapters)
HIGHLIGHTS = "highlights"  # replay buffer only: save a clip on kills / hotkey
OFF = "off"                # do nothing
ASK = "ask"                # ask the first time the game is seen
MODES = (FULL, HIGHLIGHTS, OFF)

_ALIASES = {
    "full": FULL, "session": FULL, "whole": FULL, "record": FULL, "all": FULL,
    "highlights": HIGHLIGHTS, "highlight": HIGHLIGHTS, "clips": HIGHLIGHTS, "replay": HIGHLIGHTS,
    "off": OFF, "nothing": OFF, "none": OFF, "no": OFF,
    "ask": ASK,
}

DIALOG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ask_dialog.ps1")
CREATE_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0


def parse_modes(text: str) -> dict[str, str]:
    """Parse lines like ``Counter-Strike 2 = full``. Keys are lower-cased game names."""
    modes: dict[str, str] = {}
    for raw in (text or "").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, _, mode = line.rpartition("=")
        mode = _ALIASES.get(mode.strip().lower())
        if name.strip() and mode and mode != ASK:
            modes[name.strip().lower()] = mode
    return modes


def ask_with_dialog(game: str, on_result: Callable[[str, str | None], None], log=print) -> Callable[[], None]:
    """Show the choice window without blocking OBS; ``on_result`` runs on a worker thread.

    Returns a function that closes the window (when the question was answered elsewhere).
    """
    state: dict = {"proc": None, "cancelled": False}

    def run() -> None:
        choice: str | None = None
        try:
            proc = state["proc"] = subprocess.Popen(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", DIALOG],
                env={**os.environ, "OBSPLAYED_GAME": game},
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, creationflags=CREATE_NO_WINDOW,
            )
            out, _ = proc.communicate(timeout=3600)
            lines = (out or "").strip().splitlines()
            choice = lines[-1].strip() if lines else None
        except (OSError, subprocess.TimeoutExpired) as exc:
            log(f"could not show the game prompt: {exc}")
        if not state["cancelled"]:
            on_result(game, choice if choice in MODES else None)

    def cancel() -> None:
        state["cancelled"] = True
        proc = state["proc"]
        if proc and proc.poll() is None:
            proc.kill()

    threading.Thread(target=run, name="obsplayed-ask", daemon=True).start()
    return cancel
