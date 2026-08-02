#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest


MODULE_PATH = (
    Path(__file__).resolve().parents[1]
    / "src-tauri/resources/omlx-pgrn/ssd_cache_budget.py"
)
SPEC = importlib.util.spec_from_file_location("omlx_ssd_cache_budget", MODULE_PATH)
assert SPEC and SPEC.loader
budget = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = budget
SPEC.loader.exec_module(budget)

GIB = 1024**3


class OmlxSsdCacheBudgetTest(unittest.TestCase):
    def test_low_free_space_caps_auto_budget_after_reserve(self) -> None:
        self.assertEqual(
            budget.calculate_budget_bytes(
                total_bytes=460 * GIB,
                free_bytes=10 * GIB,
                existing_cache_bytes=0,
                reserve_bytes=3 * GIB,
            ),
            7 * GIB,
        )

    def test_upstream_ten_percent_auto_limit_still_wins_with_space(self) -> None:
        self.assertEqual(
            budget.calculate_budget_bytes(
                total_bytes=460 * GIB,
                free_bytes=100 * GIB,
                existing_cache_bytes=0,
                reserve_bytes=3 * GIB,
            ),
            46 * GIB,
        )

    def test_existing_cache_counts_toward_total_safe_budget(self) -> None:
        self.assertEqual(
            budget.calculate_budget_bytes(
                total_bytes=460 * GIB,
                free_bytes=5 * GIB,
                existing_cache_bytes=4 * GIB,
                reserve_bytes=3 * GIB,
            ),
            6 * GIB,
        )

    def test_no_free_space_above_reserve_disables_cache(self) -> None:
        self.assertEqual(
            budget.calculate_budget_bytes(
                total_bytes=460 * GIB,
                free_bytes=3 * GIB,
                existing_cache_bytes=4 * GIB,
                reserve_bytes=3 * GIB,
            ),
            0,
        )

    def test_explicit_settings_are_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            (base / "settings.json").write_text(
                json.dumps({"cache": {"enabled": True, "ssd_cache_max_size": "5GB"}})
            )
            self.assertEqual(budget.resolve_action(base, None, 3.0), "preserve")

    def test_disabled_and_hot_only_settings_are_preserved(self) -> None:
        for cache in ({"enabled": False}, {"enabled": True, "hot_cache_only": True}):
            with self.subTest(cache=cache), tempfile.TemporaryDirectory() as raw:
                base = Path(raw)
                (base / "settings.json").write_text(json.dumps({"cache": cache}))
                self.assertEqual(budget.resolve_action(base, None, 3.0), "preserve")


if __name__ == "__main__":
    unittest.main()
