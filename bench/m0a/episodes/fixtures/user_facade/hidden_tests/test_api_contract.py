import copy
import unittest

import src
import src._users
import src.api


class UserApiContractTests(unittest.TestCase):
    def test_facades_export_the_implementation_callable(self):
        self.assertIs(src.normalize_users, src._users.normalize_users)
        self.assertIs(src.api.normalize_users, src._users.normalize_users)
        self.assertEqual(src.api.__all__, ["normalize_users"])
        self.assertEqual(src.__all__, ["normalize_users"])

    def test_validation_uniqueness_and_fresh_results(self):
        invalid = [
            [], {"id": 1, "name": "x", "extra": 1},
            {"id": True, "name": "x"}, {"id": 0, "name": "x"},
            {"id": 1, "name": "   "},
        ]
        for record in invalid:
            with self.subTest(record=record), self.assertRaises(ValueError):
                src.normalize_users(iter([record]))
        with self.assertRaises(ValueError):
            src.normalize_users(iter([{"id": 1, "name": "a"}, {"id": 1, "name": "b"}]))

        records = [{"id": 2, "name": " B "}, {"id": 1, "name": "A"}]
        before = copy.deepcopy(records)
        result = src.normalize_users(iter(records))
        self.assertEqual(records, before)
        self.assertIsNot(result[0], records[1])

