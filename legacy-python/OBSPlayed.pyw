"""OBSPlayed - automatic game recording and highlights (standalone app).

Double-click to start (or run: pythonw OBSPlayed.pyw). Needs OBS Studio installed;
OBSPlayed runs its own hidden copy of it only while you play.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from obsplayed_core.app import main  # noqa: E402

main()
