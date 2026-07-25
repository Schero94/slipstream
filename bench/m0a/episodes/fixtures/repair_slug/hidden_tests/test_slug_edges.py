import unittest

from src.slug import slugify


class SlugEdgeTests(unittest.TestCase):
    def test_umlauts_and_sharp_s(self):
        self.assertEqual(slugify("Grüße aus Köln"), "gruesse-aus-koeln")

    def test_no_edge_hyphens(self):
        self.assertEqual(slugify(" --Hello-- "), "hello")

    def test_empty_after_normalization(self):
        self.assertEqual(slugify("!!!"), "")
