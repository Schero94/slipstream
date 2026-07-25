import unittest

import src
from src.api import normalize_users


class UserApiTests(unittest.TestCase):
    def test_normalizes_and_sorts(self):
        records = iter([{"id": 2, "name": " Bea "}, {"id": 1, "name": "Ada"}])
        self.assertEqual(normalize_users(records), [
            {"id": 1, "name": "Ada"}, {"id": 2, "name": "Bea"},
        ])

    def test_public_exports_remain_available(self):
        self.assertIs(src.normalize_users, normalize_users)
        self.assertEqual(src.__all__, ["normalize_users"])
