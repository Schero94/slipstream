import unittest

from src.leases import LeaseBook


class Clock:
    def __init__(self, value=0.0):
        self.value = value

    def __call__(self):
        return self.value


class WorkerLeaseTests(unittest.TestCase):
    def test_acquire_renew_release_and_active_order(self):
        clock = Clock(10.0)
        book = LeaseBook(2, clock)
        self.assertEqual(book.acquire("b", 5), 15.0)
        clock.value = 11.0
        self.assertEqual(book.acquire("a", 2), 13.0)
        self.assertEqual(book.active(), [("b", 15.0), ("a", 13.0)])
        clock.value = 12.0
        self.assertEqual(book.renew("a", 4), 16.0)
        self.assertIsNone(book.release("b"))
        self.assertEqual(book.active(), [("a", 16.0)])

    def test_expired_is_reported_once(self):
        clock = Clock()
        book = LeaseBook(2, clock)
        book.acquire("a", 2)
        book.acquire("b", 1)
        clock.value = 3
        self.assertEqual(book.expired(), ["b", "a"])
        self.assertEqual(book.expired(), [])
