import unittest
from src.tree import flatten

class TreeTests(unittest.TestCase):
    def test_nested_paths(self):
        self.assertEqual(flatten({"app": {"host": "x", "port": 80}}), {"app/host": "x", "app/port": 80})
    def test_leaf_identity(self):
        leaf = []
        self.assertIs(flatten({"leaf": leaf})["leaf"], leaf)
