import unittest

from src.slug import slugify


class SlugTests(unittest.TestCase):
    def test_words_and_punctuation(self):
        self.assertEqual(slugify("Local Coding, Fast!"), "local-coding-fast")

    def test_collapses_separators(self):
        self.assertEqual(slugify("one___two   three"), "one-two-three")
