"""Game events emitted by highlight providers."""
from __future__ import annotations

import queue
import time
from dataclasses import dataclass, field

# How "hot" each kind of event is. A highlight is a cluster of events whose
# summed weight reaches the configured minimum score.
WEIGHTS: dict[str, float] = {
    "kill": 1.0,
    "headshot": 1.2,
    "assist": 0.3,
    "death": 0.0,
    "double_kill": 1.0,
    "triple_kill": 2.0,
    "quad_kill": 3.0,
    "penta_kill": 5.0,
    "ace": 4.0,
    "first_blood": 1.0,
    "objective": 1.5,
    "objective_steal": 4.0,
    "structure": 0.5,
    "mvp": 1.0,
    "manual": 10.0,
}

LABELS: dict[str, str] = {
    "kill": "Kill",
    "headshot": "Headshot",
    "assist": "Assist",
    "death": "Death",
    "double_kill": "Double Kill",
    "triple_kill": "Triple Kill",
    "quad_kill": "Quadra Kill",
    "penta_kill": "Penta Kill",
    "ace": "Ace",
    "first_blood": "First Blood",
    "objective": "Objective",
    "objective_steal": "Objective Steal",
    "structure": "Structure",
    "mvp": "MVP",
    "manual": "Manual Highlight",
}

KILL_KINDS = {"kill", "headshot"}


@dataclass
class GameEvent:
    kind: str
    game: str
    detail: str = ""
    t: float = field(default_factory=time.monotonic)
    weight: float | None = None

    def __post_init__(self) -> None:
        if self.weight is None:
            self.weight = WEIGHTS.get(self.kind, 1.0)

    @property
    def label(self) -> str:
        return LABELS.get(self.kind, self.kind.replace("_", " ").title())


class EventBus:
    """Thread-safe hand-off from provider threads to the OBS main thread."""

    def __init__(self) -> None:
        self._q: queue.Queue[GameEvent] = queue.Queue()

    def emit(self, event: GameEvent) -> None:
        self._q.put(event)

    def drain(self) -> list[GameEvent]:
        out: list[GameEvent] = []
        while True:
            try:
                out.append(self._q.get_nowait())
            except queue.Empty:
                return out
