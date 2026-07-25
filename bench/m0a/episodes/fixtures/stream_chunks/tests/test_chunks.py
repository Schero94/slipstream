import unittest

from src.chunks import chunked


class ChunkTests(unittest.TestCase):
    def test_sequence(self):
        self.assertEqual(list(chunked([1, 2, 3, 4, 5], 2)), [[1, 2], [3, 4], [5]])

    def test_large_chunk(self):
        self.assertEqual(list(chunked([1, 2], 5)), [[1, 2]])
