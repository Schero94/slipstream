import unittest
from src.lru import simulate_lru

class LruTests(unittest.TestCase):
    def test_hits_and_eviction(self):
        self.assertEqual(simulate_lru(2, ["a", "b", "a", "c"]), {"hits": 1, "misses": 3, "evictions": ["b"], "keys": ["a", "c"]})
