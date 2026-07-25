import unittest
from src.orders import transition

class OrderContractTests(unittest.TestCase):
    def test_terminal_and_illegal_transitions_rejected(self):
        for pair in (("shipped", "cancel"), ("cancelled", "pay"), ("pending", "ship"), ("paid", "pay")):
            with self.subTest(pair=pair), self.assertRaises(ValueError):
                transition(*pair)

    def test_unknown_and_non_string_inputs_rejected(self):
        for pair in (("missing", "pay"), ("pending", "missing"), (None, "pay"), ("pending", 1)):
            with self.subTest(pair=pair), self.assertRaises(ValueError):
                transition(*pair)
