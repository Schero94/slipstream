import unittest

from src.cache import invalidate_cache
from src.graph import validate_graph


class BuildInvalidationTests(unittest.TestCase):
    def test_invalidates_transitive_dependants(self):
        graph = {"core": [], "api": ["core"], "app": ["api"], "docs": []}
        cache = {"core": 1, "api": 2, "app": 3, "docs": 4}
        self.assertEqual(invalidate_cache(graph, cache, iter(["core"])), {"docs": 4})

    def test_validates_graph(self):
        self.assertIs(validate_graph({"a": [], "b": ["a"]}), None)
        with self.assertRaises(ValueError):
            validate_graph({"a": ["b"], "b": ["a"]})
