import unittest
from src.unique import unique_by

class OneShot:
    def __init__(self, values):
        self.values = iter(values)
        self.calls = 0
    def __iter__(self):
        self.calls += 1
        if self.calls > 1:
            raise AssertionError("iterated twice")
        return self.values

class UniqueContractTests(unittest.TestCase):
    def test_one_pass_one_key_call_and_identity(self):
        first, duplicate = {"id": 1}, {"id": 1}
        source = OneShot([first, duplicate, {"id": 2}])
        calls = []
        result = unique_by(source, lambda item: calls.append(item) or item["id"])
        self.assertIs(result[0], first)
        self.assertEqual(len(calls), 3)
        self.assertEqual(source.calls, 1)

    def test_unhashable_key_propagates(self):
        with self.assertRaises(TypeError):
            unique_by([1], lambda item: [item])
