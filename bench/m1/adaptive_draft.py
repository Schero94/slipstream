"""Reference implementation and evidence helpers for S3 adaptive drafting."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class AdaptiveDraftSample:
    used: int
    next: int
    acceptance: float
    ewma: float


class AdaptiveDraftController:
    """Response-level EWMA controller mirrored by the native server path."""

    def __init__(
        self,
        *,
        minimum: int = 4,
        maximum: int = 12,
        start: int = 8,
        alpha: float = 0.25,
    ) -> None:
        if not minimum <= start <= maximum:
            raise ValueError("start must be within the controller bounds")
        if not 0.0 < alpha <= 1.0:
            raise ValueError("alpha must be in (0, 1]")
        self.minimum = minimum
        self.maximum = maximum
        self.start = start
        self.alpha = alpha
        self.reset()

    def reset(self) -> None:
        self.current = self.start
        self.ewma: float | None = None

    def observe(self, *, generated: int, accepted: int) -> AdaptiveDraftSample:
        if generated <= 0 or accepted < 0 or accepted > generated:
            raise ValueError("acceptance counts must satisfy 0 <= accepted <= generated")
        used = self.current
        acceptance = accepted / generated
        if self.ewma is None:
            self.ewma = acceptance
        else:
            self.ewma = self.alpha * acceptance + (1.0 - self.alpha) * self.ewma
        if self.ewma < 0.70:
            self.current = max(self.minimum, self.current - 1)
        elif self.ewma > 0.85:
            self.current = min(self.maximum, self.current + 1)
        return AdaptiveDraftSample(
            used=used,
            next=self.current,
            acceptance=acceptance,
            ewma=self.ewma,
        )
