import unittest
from src.orders import transition

class OrderTests(unittest.TestCase):
    def test_happy_path(self):
        self.assertEqual(transition("pending", "pay"), "paid")
        self.assertEqual(transition("paid", "ship"), "shipped")

    def test_cancel_active_order(self):
        self.assertEqual(transition("pending", "cancel"), "cancelled")
        self.assertEqual(transition("paid", "cancel"), "cancelled")
