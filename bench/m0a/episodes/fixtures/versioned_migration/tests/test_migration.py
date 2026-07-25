import unittest

from src.migration import migrate_document


class MigrationTests(unittest.TestCase):
    def test_migrates_v1_and_v2(self):
        self.assertEqual(migrate_document({"version": 1, "name": "Doc"}), {
            "version": 3, "title": "Doc", "tags": [], "metadata": {"archived": False},
        })
        self.assertEqual(migrate_document({
            "version": 2, "title": "Doc", "tags": ["a"], "archived": True,
        }), {"version": 3, "title": "Doc", "tags": ["a"], "metadata": {"archived": True}})

    def test_canonicalizes_v3(self):
        source = {"version": 3, "title": "X", "tags": ["a"], "metadata": {"archived": False}}
        self.assertEqual(migrate_document(source), source)

