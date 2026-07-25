import math
import unittest

from src.batches import weighted_batches


class BatchContractTests(unittest.TestCase):
    def test_one_shot_identity_and_one_weight_call(self):
        items = [object(), object(), object()]
        calls = []
        result = weighted_batches(iter(items), lambda item: calls.append(item) or 1, 2)
        self.assertEqual(calls, items)
        self.assertIs(result[0][0], items[0])
        self.assertEqual([len(batch) for batch in result], [2, 1])

    def test_rejects_invalid_limits_and_weights(self):
        for limit in (0, True, math.inf):
            with self.subTest(limit=limit), self.assertRaises(ValueError):
                weighted_batches([], lambda item: 1, limit)
        for bad in (0, True, math.nan, 3):
            with self.subTest(weight=bad), self.assertRaises(ValueError):
                weighted_batches([bad], lambda item: item, 2)
