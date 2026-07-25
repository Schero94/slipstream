import unittest

from src.join import inner_join


class JoinContractTests(unittest.TestCase):
    def test_iterators_inputs_unchanged_and_outputs_unaliased(self):
        left = [{"id": "a", "x": 1}]
        right = [{"id": "a", "x": 1, "y": 2}]
        result = inner_join(iter(left), iter(right))
        self.assertEqual(result, [{"id": "a", "x": 1, "y": 2}])
        self.assertIsNot(result[0], left[0])
        self.assertIsNot(result[0], right[0])

    def test_rejects_duplicates_malformed_and_boolean_ids(self):
        cases = (([{"id": 1}, {"id": 1}], []), ([{"x": 1}], []), ([{"id": True}], []))
        for left, right in cases:
            with self.subTest(left=left), self.assertRaises(ValueError):
                inner_join(left, right)

    def test_unhashable_id_propagates_type_error(self):
        with self.assertRaises(TypeError):
            inner_join([{"id": []}], [])
