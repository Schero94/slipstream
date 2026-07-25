import unittest
from src.config import overlay

class ConfigContractTests(unittest.TestCase):
    def test_no_mutation_and_schema_only(self):
        defaults = {"debug": False, "name": "old", "custom": 7}
        env = {"PGR_NAME": "new", "PGR_CUSTOM": "9", "PGR_UNKNOWN": "x"}
        self.assertEqual(overlay(defaults, env), {"debug": False, "name": "new", "custom": 7})
        self.assertEqual(defaults["name"], "old")
    def test_converter_error_propagates(self):
        with self.assertRaises(ValueError): overlay({}, {"PGR_PORT": "70000"})
