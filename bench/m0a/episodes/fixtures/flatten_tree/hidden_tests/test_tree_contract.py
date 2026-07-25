import unittest
from src.tree import flatten

class TreeContractTests(unittest.TestCase):
    def test_order_and_deep_paths(self):
        self.assertEqual(list(flatten({"z": {"a": 1}, "b": 2})), ["z/a", "b"])
    def test_invalid_roots_keys_and_empty_branches(self):
        for tree in ([], {"": 1}, {"a/b": 1}, {1: 2}, {"empty": {}}):
            with self.subTest(tree=tree), self.assertRaises(ValueError): flatten(tree)
