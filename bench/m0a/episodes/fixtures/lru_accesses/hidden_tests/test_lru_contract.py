import unittest
from src.lru import simulate_lru

class LruContractTests(unittest.TestCase):
    def test_recency_and_one_shot(self):
        self.assertEqual(simulate_lru(3, iter([1, 2, 3, 1, 4]))["evictions"], [2])
        self.assertEqual(simulate_lru(3, [1, 2, 3, 1])["keys"], [2, 3, 1])
    def test_invalid_capacity_and_key(self):
        for capacity in (True, 0, -1, 1.5):
            with self.subTest(capacity=capacity), self.assertRaises(ValueError): simulate_lru(capacity, [])
        with self.assertRaises(TypeError): simulate_lru(1, [[]])
