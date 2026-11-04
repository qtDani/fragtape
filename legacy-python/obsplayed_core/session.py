"""Recording sessions, timeline markers and highlight detection."""
from __future__ import annotations

import time
from dataclasses import asdict, dataclass, field

from .events import KILL_KINDS, LABELS, GameEvent


@dataclass
class Marker:
    offset: float  # seconds since the recording started
    kind: str
    label: str
    game: str
    detail: str
    weight: float


@dataclass
class Highlight:
    start: float
    end: float
    score: float
    title: str
    markers: list[Marker]

    @property
    def duration(self) -> float:
        return self.end - self.start


@dataclass
class Session:
    started_mono: float
    started_wall: float = field(default_factory=time.time)
    game: str | None = None
    markers: list[Marker] = field(default_factory=list)
    chapters_ok: bool = True  # every marker was written as a chapter in the video

    def add(self, event: GameEvent) -> Marker:
        marker = Marker(
            offset=round(max(0.0, event.t - self.started_mono), 2),
            kind=event.kind,
            label=event.label,
            game=event.game,
            detail=event.detail,
            weight=float(event.weight or 0.0),
        )
        self.markers.append(marker)
        if self.game is None:
            self.game = event.game
        return marker

    def to_dict(self, recording: str | None, highlights: list[Highlight]) -> dict:
        return {
            "version": 1,
            "game": self.game,
            "recording": recording,
            "started": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(self.started_wall)),
            "markers": [asdict(m) for m in self.markers],
            "highlights": [
                {
                    "start": round(h.start, 2),
                    "end": round(h.end, 2),
                    "score": round(h.score, 2),
                    "title": h.title,
                    "markers": [asdict(m) for m in h.markers],
                }
                for h in highlights
            ],
        }


def title_for(markers: list[Marker]) -> str:
    best = max(markers, key=lambda m: m.weight)
    kills = sum(1 for m in markers if m.kind in KILL_KINDS)
    title = LABELS.get(best.kind, best.label)
    if best.kind in KILL_KINDS:
        if kills >= 2:
            return f"{kills} Kills"
        return title
    if kills >= 2:
        title += f" ({kills} kills)"
    return title


def find_highlights(
    markers: list[Marker],
    *,
    merge_gap: float = 10.0,
    min_score: float = 2.0,
    pre: float = 8.0,
    post: float = 4.0,
    duration: float | None = None,
) -> list[Highlight]:
    """Group nearby markers into clusters and keep the "hot" ones.

    Events closer than ``merge_gap`` seconds belong to the same moment. A
    moment becomes a highlight when the sum of its weights reaches
    ``min_score``. Each clip spans ``pre`` seconds before the first event to
    ``post`` seconds after the last; overlapping clips are merged.
    """
    ordered = sorted(markers, key=lambda m: m.offset)
    clusters: list[list[Marker]] = []
    for m in ordered:
        if clusters and m.offset - clusters[-1][-1].offset <= merge_gap:
            clusters[-1].append(m)
        else:
            clusters.append([m])

    out: list[Highlight] = []
    for cluster in clusters:
        score = sum(m.weight for m in cluster)
        if score < min_score:
            continue
        start = max(0.0, cluster[0].offset - pre)
        end = cluster[-1].offset + post
        if duration is not None:
            end = min(end, duration)
        if out and start <= out[-1].end:
            prev = out[-1]
            merged = prev.markers + cluster
            out[-1] = Highlight(prev.start, max(prev.end, end), prev.score + score, title_for(merged), merged)
        else:
            out.append(Highlight(start, end, score, title_for(cluster), cluster))
    return out
