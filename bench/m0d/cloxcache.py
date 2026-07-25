"""cloxcache — CLOCK-LRU-K hybrid eviction (adopted from colibri PR #223).

colibri #223 replaced plain LRU with a "frequency-aware recency" policy that adapts
between LFU and LRU with frequency decay, reporting +60% expert-cache hit-rate — but
ONLY in the tight-cache regime (cache < ~1/3 of per-token expert demand); it is inert
(identical to LRU) when the cache is large. Our 35B at 10 GB is the large-cache regime
(~95%, where LRU already wins), but a flagship with more experts at the same RAM lands
in the tighter regime where this policy can matter. We reimplement it to measure that.

CLOCK-LRU-K: each resident key holds a counter in [0, K]. A hit bumps the counter
(capped at K = frequency memory). Eviction sweeps a clock hand, decrementing counters
(the decay = recency pressure) and evicting the first key that reaches 0. K=1 ≈ pure
CLOCK/second-chance (recency); larger K leans LFU. O(1) amortized (no per-evict scan).
"""

from __future__ import annotations


class CloxCache:
    def __init__(self, capacity: int, *, k: int = 3):
        if capacity < 1:
            raise ValueError("capacity must be >= 1")
        if k < 1:
            raise ValueError("k must be >= 1")
        self.capacity = capacity
        self.k = k
        self._ring: list = []           # keys in clock order (grows to capacity, then fixed)
        self._count: dict = {}          # key -> counter in [0, k]
        self._hand = 0

    def __len__(self) -> int:
        return len(self._ring)

    def __contains__(self, key) -> bool:
        return key in self._count

    def access(self, key) -> bool:
        """Get-or-insert. Returns True on a hit (already resident), False on a miss."""
        if key in self._count:
            c = self._count[key]
            if c < self.k:
                self._count[key] = c + 1
            return True
        # miss -> insert, evicting via CLOCK sweep when full
        if len(self._ring) < self.capacity:
            self._ring.append(key)
            self._count[key] = 1
        else:
            while True:
                victim = self._ring[self._hand]
                if self._count[victim] > 0:
                    self._count[victim] -= 1
                    self._hand = (self._hand + 1) % self.capacity
                else:
                    del self._count[victim]
                    self._ring[self._hand] = key
                    self._count[key] = 1
                    self._hand = (self._hand + 1) % self.capacity
                    break
        return False
