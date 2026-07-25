import unittest

from solution import chunked


class ChunkedTests(unittest.TestCase):
    def test_consecutive_chunks_and_short_tail(self):
        self.assertEqual(chunked([1, 2, 3, 4, 5], 2), [[1, 2], [3, 4], [5]])

    def test_accepts_iterators(self):
        self.assertEqual(chunked(iter(range(4)), 3), [[0, 1, 2], [3]])

    def test_rejects_non_positive_size(self):
        for size in (0, -1):
            with self.subTest(size=size), self.assertRaises(ValueError):
                chunked([], size)
