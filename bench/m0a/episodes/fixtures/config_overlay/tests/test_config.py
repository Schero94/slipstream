import unittest
from src.config import overlay

class ConfigTests(unittest.TestCase):
    def test_recognized_overrides(self):
        self.assertEqual(overlay({"debug": False, "port": 80}, {"PGR_DEBUG": "true", "PGR_PORT": "8080"}), {"debug": True, "port": 8080})
    def test_ignores_unrelated(self):
        self.assertEqual(overlay({"name": "a"}, {"OTHER": "b"}), {"name": "a"})
