"""The clip library: every recording and clip under the OBSPlayed videos folder.

Videos live in ``Videos\\OBSPlayed\\<Game>\\``. What OBSPlayed knows about each
one (game, title, highlight markers, duration) is kept in ``clips.json`` in the
app's data folder, so nothing extra is written next to your videos. Files added
or deleted by hand show up / disappear on the next listing.
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

from . import clipper

VIDEO_EXTS = {".mp4", ".mov", ".mkv"}
NO_WINDOW = 0x08000000


def videos_root() -> Path:
    return Path.home() / "Videos" / "OBSPlayed"


def clip_id(rel: str) -> str:
    return hashlib.sha1(rel.lower().encode("utf-8")).hexdigest()[:16]


class ClipStore:
    def __init__(self, root: Path, data_dir: Path, log=print) -> None:
        self.root = root
        self.meta_path = data_dir / "clips.json"
        self.thumbs = data_dir / "thumbs"
        self.log = log
        self._lock = threading.RLock()
        try:
            self.meta: dict[str, dict] = json.loads(self.meta_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self.meta = {}
        self._index: dict[str, Path] = {}

    # ---- persistence -----------------------------------------------------------
    def _save(self) -> None:
        self.meta_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.meta_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self.meta, indent=1), encoding="utf-8")
        os.replace(tmp, self.meta_path)

    def _rel(self, path: Path) -> str:
        return path.resolve().relative_to(self.root.resolve()).as_posix()

    # ---- adding --------------------------------------------------------------------
    def add(self, info: dict) -> None:
        """Called by the controller when a recording or clip is finished."""
        path = Path(info["path"])
        try:
            rel = self._rel(path)
        except (ValueError, OSError):
            return  # outside the library folder
        entry = {k: info[k] for k in ("kind", "game", "title", "markers", "highlights", "started") if info.get(k)}
        with self._lock:
            self.meta.setdefault(rel, {}).update(entry)
            self._save()

    # ---- listing -------------------------------------------------------------------
    def list(self, live: set[str] | frozenset = frozenset()) -> list[dict]:
        """All videos, newest first. ``live`` = paths still being recorded."""
        live_keys = {os.path.normcase(os.path.abspath(p)) for p in live if p}
        out: list[dict] = []
        index: dict[str, Path] = {}
        if self.root.exists():
            for dirpath, _dirs, files in os.walk(self.root):
                for name in files:
                    p = Path(dirpath) / name
                    if p.suffix.lower() not in VIDEO_EXTS:
                        continue
                    try:
                        st = p.stat()
                    except OSError:
                        continue
                    rel = p.relative_to(self.root).as_posix()
                    cid = clip_id(rel)
                    index[cid] = p
                    m = self.meta.get(rel, {})
                    is_live = os.path.normcase(os.path.abspath(p)) in live_keys
                    # A duration measured while the file was still growing doesn't count.
                    duration = m.get("duration") if m.get("dur_mtime") == int(st.st_mtime) and not is_live else None
                    parts = rel.split("/")
                    kind = m.get("kind") or ("clip" if p.stem.startswith(("Clip", "Replay")) or "Highlights" in rel
                                             else "session")
                    out.append({
                        "id": cid, "rel": rel, "name": p.name,
                        "game": m.get("game") or (parts[0] if len(parts) > 1 else "Other"),
                        "kind": kind,
                        "title": m.get("title") or _title_from_name(p.stem),
                        "size": st.st_size, "mtime": int(st.st_mtime),
                        "duration": duration,
                        "live": is_live,
                        "markers": [{"offset": x["offset"], "label": x["label"], "detail": x.get("detail", "")}
                                    for x in m.get("markers", [])],
                        "highlights": [{"start": h["start"], "end": h["end"], "title": h["title"]}
                                       for h in m.get("highlights", [])],
                        "thumb": (self.thumbs / f"{cid}.jpg").exists(),
                        "favorite": bool(m.get("favorite")),
                    })
        with self._lock:
            self._index = index
        out.sort(key=lambda c: c["mtime"], reverse=True)
        return out

    def path_for(self, cid: str) -> Path | None:
        with self._lock:
            p = self._index.get(cid)
        if p is None:
            self.list()
            with self._lock:
                p = self._index.get(cid)
        return p if p and p.exists() else None

    def _meta_for(self, cid: str) -> tuple[str, dict] | None:
        p = self.path_for(cid)
        if not p:
            return None
        rel = p.relative_to(self.root).as_posix()
        return rel, self.meta.setdefault(rel, {})

    # ---- editing -------------------------------------------------------------------
    def update(self, cid: str, **fields: Any) -> None:
        with self._lock:
            hit = self._meta_for(cid)
            if not hit:
                raise KeyError(cid)
            allowed = {k: v for k, v in fields.items() if k in ("title", "favorite", "duration")}
            if "duration" in allowed:
                try:
                    allowed["dur_mtime"] = int(self.path_for(cid).stat().st_mtime)
                except (AttributeError, OSError):
                    pass
            hit[1].update(allowed)
            self._save()

    def save_thumb(self, cid: str, jpeg: bytes) -> None:
        if not self.path_for(cid) or not jpeg.startswith(b"\xff\xd8"):
            raise ValueError("not a jpeg")
        self.thumbs.mkdir(parents=True, exist_ok=True)
        (self.thumbs / f"{cid}.jpg").write_bytes(jpeg)

    def delete(self, cid: str) -> None:
        """Move the video to the Recycle Bin (so a mis-click can be undone)."""
        p = self.path_for(cid)
        if not p:
            raise KeyError(cid)
        _recycle(p)
        with self._lock:
            self.meta.pop(p.relative_to(self.root).as_posix(), None)
            self._save()
        try:
            (self.thumbs / f"{cid}.jpg").unlink()
        except OSError:
            pass

    def trim(self, cid: str, start: float, end: float, ffmpeg: str | None, title: str = "") -> dict:
        """Save [start, end] as a new clip next to the original (stream copy, no re-encode)."""
        src = self.path_for(cid)
        if not src:
            raise KeyError(cid)
        if not ffmpeg:
            raise RuntimeError("ffmpeg is needed to save clips (winget install Gyan.FFmpeg)")
        if end - start < 1:
            raise ValueError("the clip must be at least 1 second long")
        with self._lock:
            info = dict(self.meta.get(src.relative_to(self.root).as_posix(), {}))
        name = title.strip() or "Clip"
        stamp = time.strftime("%Y-%m-%d %H-%M-%S")
        dst = src.with_name(f"Clip {stamp} - {clipper.safe_name(name)}.mp4")
        cmd = [ffmpeg, "-hide_banner", "-loglevel", "error", "-y", "-ss", f"{start:.3f}", "-i", str(src),
               "-t", f"{end - start:.3f}", "-map", "0:v:0", "-map", "0:a?", "-c", "copy",
               "-avoid_negative_ts", "make_zero", "-movflags", "+faststart", str(dst)]
        r = subprocess.run(cmd, capture_output=True, text=True, creationflags=NO_WINDOW if sys.platform == "win32" else 0)
        if r.returncode != 0 or not dst.exists():
            raise RuntimeError((r.stderr or "ffmpeg failed").strip()[-300:])
        markers = [dict(m, offset=round(m["offset"] - start, 2)) for m in info.get("markers", [])
                   if start <= m["offset"] <= end]
        self.add({"kind": "clip", "path": str(dst), "game": info.get("game"), "title": name, "markers": markers})
        return {"id": clip_id(self._rel(dst)), "name": dst.name}


def _title_from_name(stem: str) -> str:
    # "Clip 2026-11-03 21-00-00 - Triple Kill" -> "Triple Kill"; plain timestamps stay as they are.
    if " - " in stem:
        return stem.split(" - ", 1)[1]
    return stem


def _recycle(path: Path) -> None:
    if sys.platform != "win32":
        path.unlink()
        return
    import ctypes
    from ctypes import wintypes

    class SHFILEOPSTRUCTW(ctypes.Structure):
        _fields_ = [("hwnd", wintypes.HWND), ("wFunc", wintypes.UINT), ("pFrom", wintypes.LPCWSTR),
                    ("pTo", wintypes.LPCWSTR), ("fFlags", ctypes.c_ushort), ("fAnyOperationsAborted", wintypes.BOOL),
                    ("hNameMappings", ctypes.c_void_p), ("lpszProgressTitle", wintypes.LPCWSTR)]

    op = SHFILEOPSTRUCTW(None, 3, str(path) + "\0", None, 0x0040 | 0x0010 | 0x0004 | 0x0400)  # DELETE, ALLOWUNDO|NOCONFIRMATION|SILENT|NOERRORUI
    rc = ctypes.windll.shell32.SHFileOperationW(ctypes.byref(op))
    if rc != 0 or path.exists():
        raise OSError(f"could not move {path.name} to the Recycle Bin (code {rc})")
