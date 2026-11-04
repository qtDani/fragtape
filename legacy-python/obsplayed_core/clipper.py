"""Cut highlight clips out of a finished recording with ffmpeg."""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

from .session import Highlight

CREATE_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0


def find_ffmpeg(custom: str = "") -> str | None:
    if custom and Path(custom).is_file():
        return custom
    found = shutil.which("ffmpeg")
    if found:
        return found
    local = os.environ.get("LOCALAPPDATA", "")
    for candidate in (
        Path(local) / "Microsoft" / "WinGet" / "Links" / "ffmpeg.exe",
        Path(os.environ.get("USERPROFILE", "")) / "scoop" / "shims" / "ffmpeg.exe",
        Path(r"C:\ffmpeg\bin\ffmpeg.exe"),
        Path(r"C:\ProgramData\chocolatey\bin\ffmpeg.exe"),
    ):
        if candidate.is_file():
            return str(candidate)
    return None


def safe_name(text: str) -> str:
    return re.sub(r'[<>:"/\\|?*\x00-\x1f]+', "", text).strip() or "highlight"


def clips_dir(recording: str) -> Path:
    rec = Path(recording)
    return rec.parent / "OBSPlayed Highlights" / rec.stem


def cut_clips(ffmpeg: str, recording: str, highlights: list[Highlight], log=print) -> list[str]:
    out_dir = clips_dir(recording)
    out_dir.mkdir(parents=True, exist_ok=True)
    written: list[str] = []
    for i, h in enumerate(highlights, 1):
        out = out_dir / f"{i:02d} - {safe_name(h.title)}.mp4"
        cmd = [
            ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
            "-ss", f"{h.start:.2f}", "-i", recording, "-t", f"{h.duration:.2f}",
            "-map", "0:v?", "-map", "0:a?", "-c", "copy",
            "-avoid_negative_ts", "make_zero", "-movflags", "+faststart",
            str(out),
        ]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600, creationflags=CREATE_NO_WINDOW)
        except (OSError, subprocess.TimeoutExpired) as exc:
            log(f"[clips] ffmpeg failed: {exc}")
            continue
        if proc.returncode == 0:
            written.append(str(out))
        else:
            log(f"[clips] ffmpeg error for {out.name}: {proc.stderr.strip()[:300]}")
    return written
