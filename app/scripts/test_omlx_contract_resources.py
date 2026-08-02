#!/usr/bin/env python3
"""Release-resource contract for bounded structured prompts and safe MX eviction."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
RESOURCE = ROOT / "src-tauri" / "resources" / "omlx-pgrn"
PROFILE = RESOURCE / "omlx" / "pgrn" / "profile.py"
STORE = RESOURCE / "omlx" / "pgrn" / "store.py"
BOOTSTRAP = RESOURCE / "bootstrap_mlx_runtime.sh"
GRAMMAR_REQUIREMENTS = RESOURCE / "requirements-mlx-grammar.txt"


class ContractResourcesTest(unittest.TestCase):
    def test_contract_profile_is_bounded_for_structured_prompts(self) -> None:
        spec = importlib.util.spec_from_file_location("release_pgrn_profile", PROFILE)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader if spec else None)
        module = importlib.util.module_from_spec(spec)  # type: ignore[arg-type]
        sys.modules[spec.name] = module  # dataclasses resolves annotations by module name
        spec.loader.exec_module(module)  # type: ignore[union-attr]
        profile = module.resolve_profile("contract")
        self.assertEqual(profile.name, "contract")
        self.assertEqual(profile.capacity, 2048)
        self.assertEqual(profile.hot_capacity, 1024)
        self.assertEqual(profile.io_width, 16)

    def test_store_preserves_the_active_expert_bank_during_eviction(self) -> None:
        source = STORE.read_text(encoding="utf-8")
        self.assertIn("def _mx_evict_one(", source)
        self.assertIn("preserve: set[Tuple[int, int]] | None = None", source)
        self.assertIn("active_keys = {(int(layer), int(e)) for e in experts}", source)
        self.assertIn("preserve=active_keys", source)
        self.assertIn("cache is smaller than the active expert bank", source)

    def test_runtime_bootstrap_installs_and_verifies_grammar_support(self) -> None:
        pins = GRAMMAR_REQUIREMENTS.read_text(encoding="utf-8")
        self.assertIn("xgrammar==0.2.3", pins)
        self.assertIn("apache-tvm-ffi==0.1.11", pins)
        bootstrap = BOOTSTRAP.read_text(encoding="utf-8")
        self.assertIn("requirements-mlx-grammar.txt", bootstrap)
        self.assertIn("grammar_runtime_ready", bootstrap)
        self.assertIn("upgrade_required", bootstrap)
        self.assertIn("pip install --no-deps", bootstrap)


if __name__ == "__main__":
    unittest.main()
