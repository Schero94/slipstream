import unittest

from src.headers import parse_headers


class OneShot:
    def __init__(self, values):
        self.values = iter(values)
        self.count = 0

    def __iter__(self):
        self.count += 1
        if self.count > 1:
            raise AssertionError("consumed twice")
        return self.values


class HeaderContractTests(unittest.TestCase):
    def test_one_shot_and_first_separator(self):
        source = OneShot(["Location: https://x.test:8443/a"])
        self.assertEqual(parse_headers(source), {"location": ["https://x.test:8443/a"]})
        self.assertEqual(source.count, 1)

    def test_rejects_bad_names_empty_values_and_non_strings(self):
        for line in ("Bad Name: x", "Ünicode: x", "A:", "missing", 3):
            with self.subTest(line=line), self.assertRaises(ValueError):
                parse_headers([line])
