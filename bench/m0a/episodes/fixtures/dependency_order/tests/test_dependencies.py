import unittest

from src.dependencies import dependency_order


class DependencyTests(unittest.TestCase):
    def test_dependency_first_stable_order(self):
        graph = {"app": ["db", "api"], "db": [], "api": ["db"], "docs": []}
        self.assertEqual(dependency_order(graph), ["db", "api", "app", "docs"])

    def test_rejects_cycle(self):
        with self.assertRaises(ValueError):
            dependency_order({"a": ["b"], "b": ["a"]})
