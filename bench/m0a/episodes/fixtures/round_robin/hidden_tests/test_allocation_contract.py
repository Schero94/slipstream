import unittest
from src.allocation import allocate

class AllocationContractTests(unittest.TestCase):
    def test_one_shot_and_identity(self):
        first, second = object(), object()
        result = allocate(iter([first, second]), ("a", "b", "c"))
        self.assertIs(result["a"][0], first)
        self.assertIs(result["b"][0], second)
        self.assertEqual(result["c"], [])
    def test_invalid_workers(self):
        for workers in ([], ["a", "a"], [""], [1], "ab"):
            with self.subTest(workers=workers), self.assertRaises(ValueError): allocate([], workers)
