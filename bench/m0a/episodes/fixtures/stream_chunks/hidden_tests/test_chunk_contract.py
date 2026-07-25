import unittest

from src.chunks import chunked


class CountingIterator:
    def __init__(self, values):
        self.values = iter(values)
        self.consumed = 0

    def __iter__(self):
        return self

    def __next__(self):
        value = next(self.values)
        self.consumed += 1
        return value


class ChunkContractTests(unittest.TestCase):
    def test_one_shot_iterator(self):
        self.assertEqual(list(chunked(iter(range(5)), 2)), [[0, 1], [2, 3], [4]])

    def test_lazy_and_not_more_than_one_chunk_ahead(self):
        source = CountingIterator(range(6))
        chunks = chunked(source, 2)
        self.assertEqual(source.consumed, 0)
        self.assertEqual(next(chunks), [0, 1])
        self.assertEqual(source.consumed, 2)

    def test_invalid_sizes(self):
        for value in (True, 0, -1, 1.5, "2"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                list(chunked([1], value))
