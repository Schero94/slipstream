import unittest

from src.dependencies import dependency_order


class DependencyContractTests(unittest.TestCase):
    def test_ready_ties_follow_original_key_order_without_mutation(self):
        graph = {"z": [], "a": [], "end": ["z", "a"]}
        before = {key: list(value) for key, value in graph.items()}
        self.assertEqual(dependency_order(graph), ["z", "a", "end"])
        self.assertEqual(graph, before)

    def test_rejects_unknown_duplicate_and_malformed_dependencies(self):
        cases = ({"a": ["missing"]}, {"a": ["a", "a"]}, {"a": {"b"}, "b": []}, {1: []})
        for graph in cases:
            with self.subTest(graph=graph), self.assertRaises(ValueError):
                dependency_order(graph)
