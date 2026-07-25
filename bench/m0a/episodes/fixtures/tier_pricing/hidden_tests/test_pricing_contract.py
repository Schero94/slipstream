import unittest

from src.pricing import quote


class PricingContractTests(unittest.TestCase):
    def test_progressive_boundaries(self):
        self.assertEqual(quote(51, "regular")["subtotal"], 487)

    def test_internal_and_unknown_customer(self):
        self.assertEqual(quote(20, "internal")["total"], 0)
        with self.assertRaises(ValueError):
            quote(20, "guest")

    def test_invalid_quantities(self):
        for value in (True, 0, -1, 1.5, "2"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                quote(value, "regular")
