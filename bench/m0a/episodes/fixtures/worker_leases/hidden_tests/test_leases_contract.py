import math
import unittest

from src.leases import LeaseBook


class CountingClock:
    def __init__(self, value=0.0):
        self.value = value
        self.calls = 0

    def __call__(self):
        self.calls += 1
        return self.value


class WorkerLeaseContractTests(unittest.TestCase):
    def test_calls_clock_once_and_enforces_capacity_after_expiry(self):
        clock = CountingClock()
        book = LeaseBook(1, clock)
        self.assertEqual(clock.calls, 0)
        book.acquire("a", 1)
        self.assertEqual(clock.calls, 1)
        with self.assertRaises(ValueError):
            book.acquire("b", 1)
        self.assertEqual(clock.calls, 2)
        clock.value = 2
        book.acquire("b", 1)
        self.assertEqual(clock.calls, 3)
        self.assertEqual(book.expired(), ["a"])
        self.assertEqual(clock.calls, 4)

    def test_validation_and_backwards_time(self):
        for maximum in [0, True, 1.5]:
            with self.subTest(maximum=maximum), self.assertRaises(ValueError):
                LeaseBook(maximum, CountingClock())
        clock = CountingClock(5)
        book = LeaseBook(1, clock)
        for worker, ttl in [("", 1), (1, 1), ("a", 0), ("a", True), ("a", math.inf)]:
            with self.subTest(worker=worker, ttl=ttl), self.assertRaises(ValueError):
                book.acquire(worker, ttl)
        clock.value = 4
        with self.assertRaises(ValueError):
            book.active()

    def test_active_requirement_and_expiry_tie_order(self):
        clock = CountingClock()
        book = LeaseBook(3, clock)
        book.acquire("first", 2)
        book.acquire("second", 2)
        with self.assertRaises(ValueError):
            book.renew("missing", 1)
        with self.assertRaises(ValueError):
            book.release("missing")
        clock.value = 3
        self.assertEqual(book.expired(), ["first", "second"])

