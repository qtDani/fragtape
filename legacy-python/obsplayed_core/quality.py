"""Recording quality presets, translated into OBS encoder settings.

Presets use constant quality (CQP/CRF) instead of a fixed bitrate: busy game
scenes get the bits they need, menus don't waste space. Recording gets its own
encoder so it never shares the (usually low-bitrate) streaming encoder.
"""
from __future__ import annotations

PRESETS = ("keep", "small", "balanced", "high", "max")
LABELS = {"keep": "Keep my OBS settings", "small": "Smaller files", "balanced": "Balanced", "high": "High", "max": "Max"}

# Quantizer per preset: lower = better quality, bigger files.
HW_QP = {"small": 28, "balanced": 24, "high": 20, "max": 16}
X264_CRF = {"small": 26, "balanced": 22, "high": 19, "max": 16}

# Recording encoders to use, best first, per GPU vendor (H.264 plays everywhere).
CANDIDATES = {
    "nvenc": ("obs_nvenc_h264_tex", "jim_nvenc", "ffmpeg_nvenc"),
    "amd": ("h264_texture_amf",),
    "qsv": ("obs_qsv11_v2", "obs_qsv11"),
    "x264": ("obs_x264",),
}


def family(encoder_id: str) -> str:
    e = (encoder_id or "").lower()
    if "nvenc" in e:
        return "nvenc"
    if "amf" in e or "amd" in e:
        return "amd"
    if "qsv" in e:
        return "qsv"
    return "x264"


def pick_encoder(available: list[str], stream_encoder: str = "") -> str:
    """Prefer the GPU vendor the stream encoder uses, then any hardware encoder, then x264."""
    order = [family(stream_encoder)] + [f for f in ("nvenc", "amd", "qsv", "x264") if f != family(stream_encoder)]
    if family(stream_encoder) == "x264":
        order = ["nvenc", "amd", "qsv", "x264"]
    for fam in order:
        for enc in CANDIDATES[fam]:
            if enc in available:
                return enc
    return "obs_x264"


def encoder_settings(encoder_id: str, preset: str) -> dict:
    fam = family(encoder_id)
    if fam == "x264":
        return {"rate_control": "CRF", "crf": X264_CRF[preset], "preset": "veryfast", "keyint_sec": 2}
    qp = HW_QP[preset]
    if fam == "nvenc":
        return {"rate_control": "CQP", "cqp": qp, "preset2": "p5", "tune": "hq", "multipass": "qres",
                "bf": 2, "keyint_sec": 2}
    if fam == "amd":
        return {"rate_control": "CQP", "cqp": qp, "preset": "quality", "bf": 2, "keyint_sec": 2}
    return {"rate_control": "CQP", "cqp": qp, "qpi": qp, "qpp": qp, "qpb": qp,
            "target_usage": "TU2", "keyint_sec": 2}


SIMPLE_QUALITY = {"small": "Small", "balanced": "HQ", "high": "HQ", "max": "HQ"}
