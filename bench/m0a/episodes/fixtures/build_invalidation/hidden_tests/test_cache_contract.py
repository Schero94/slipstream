import copy
import unittest

from src.cache import invalidate_cache
from src.graph import validate_graph


class BuildInvalidationContractTests(unittest.TestCase):
    def test_rejects_malformed_graph_changed_and_cache(self):
        bad_graphs = [[], {"": []}, {"a": ""}, {"a": ["a"]}, {"a": ["b"]}, {"a": [], "b": ["a", "a"]}]
        for graph in bad_graphs:
            with self.subTest(graph=graph), self.assertRaises(ValueError):
                validate_graph(graph)
        graph = {"a": [], "b": ["a"]}
        for changed in [["x"], ["a", "a"], [1]]:
            with self.subTest(changed=changed), self.assertRaises(ValueError):
                invalidate_cache(graph, {"a": 1}, iter(changed))
        with self.assertRaises(ValueError):
            invalidate_cache(graph, {"outside": 1}, [])

    def test_preserves_inputs_and_cache_order(self):
        graph = {"a": [], "b": ["a"], "c": []}
        cache = {"c": [], "a": {}, "b": 3}
        before_graph, before_cache = copy.deepcopy(graph), copy.deepcopy(cache)
        result = invalidate_cache(graph, cache, iter(["a"]))
        self.assertEqual(result, {"c": []})
        self.assertEqual(list(result), ["c"])
        self.assertEqual(graph, before_graph)
        self.assertEqual(cache, before_cache)
