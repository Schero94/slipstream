import copy
import unittest

from src.migration import migrate_document
from src.schema import validate_v3


class MigrationContractTests(unittest.TestCase):
    def test_rejects_malformed_and_extra_fields(self):
        invalid = [
            [], {"version": True, "name": "x"}, {"version": 0, "name": "x"},
            {"version": 1, "name": "", "extra": 1},
            {"version": 1, "name": "x", "labels": [1]},
            {"version": 2, "title": "x", "tags": [], "archived": 1},
            {"version": 3, "title": "x", "tags": [], "metadata": {}},
        ]
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ValueError):
                migrate_document(value)

    def test_no_mutation_or_aliasing(self):
        source = {"version": 3, "title": "X", "tags": ["a"], "metadata": {"archived": False}}
        before = copy.deepcopy(source)
        result = migrate_document(source)
        self.assertEqual(source, before)
        self.assertIsNot(result, source)
        self.assertIsNot(result["tags"], source["tags"])
        self.assertIsNot(result["metadata"], source["metadata"])
        validate_v3(result)
