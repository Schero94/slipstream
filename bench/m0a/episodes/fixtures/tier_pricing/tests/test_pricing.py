import unittest

from src.pricing import quote


class PricingTests(unittest.TestCase):
    def test_first_tier_regular(self):
        self.assertEqual(quote(4, "regular")["total"], 48)

    def test_partner_discount(self):
        self.assertEqual(quote(10, "partner")["total"], 108)

    def test_progressive_pricing(self):
        self.assertEqual(quote(11, "regular")["subtotal"], 129)

    def test_return_shape(self):
        self.assertEqual(
            set(quote(1, "regular")),
            {"quantity", "customer", "subtotal", "discount", "total"},
        )
