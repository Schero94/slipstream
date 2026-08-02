#!/usr/bin/env python3
"""Release contract for Slipstream's native inference runtimes."""

from __future__ import annotations

import json
from pathlib import Path, PurePosixPath
import unittest


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "src-tauri" / "resources" / "runtime-manifest.json"
TAURI_CONFIG = ROOT / "src-tauri" / "tauri.conf.json"


class RuntimeManifestTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))

    def test_native_engine_identity_is_explicit(self) -> None:
        self.assertEqual(self.manifest["schema"], 1)
        self.assertEqual(self.manifest["product_engine"], "llama.cpp-pgrn")
        self.assertIs(self.manifest["ollama"], False)

    def test_required_llama_components_are_declared(self) -> None:
        components = self.manifest["components"]
        self.assertEqual(components["llama_server"]["path"], "llama-server")
        self.assertIs(components["llama_server"]["required"], True)
        self.assertIs(components["llama_server"]["executable"], True)
        self.assertEqual(components["pgrn_convert"]["path"], "pgrn-convert")
        self.assertIs(components["pgrn_convert"]["required"], True)
        self.assertIs(components["pgrn_convert"]["executable"], True)

    def test_omlx_components_are_apple_silicon_scoped(self) -> None:
        components = self.manifest["components"]
        required = (
            "omlx_launcher",
            "omlx_bootstrap",
            "omlx_uv",
            "omlx_runtime_lock",
            "omlx_fork",
            "omlx_cli",
            "omlx_server",
            "omlx_torch_stub",
            "omlx_pgrn_profile",
            "omlx_pgrn_store",
            "pgrn_host_python",
            "pgrn_host",
        )
        for name in required:
            self.assertEqual(components[name]["platform"], "macos-arm64")
            self.assertIs(components[name]["required"], True)
        self.assertIs(components["omlx_launcher"]["executable"], True)
        self.assertIs(components["omlx_bootstrap"]["executable"], True)
        self.assertIs(components["omlx_uv"]["executable"], True)
        self.assertEqual(
            components["omlx_launcher"]["minimum_system_version"], "14.0"
        )

    def test_mlx_sources_are_exactly_pinned(self) -> None:
        packages = self.manifest["mlx_packages"]
        self.assertEqual(packages["mlx"], "0.32.0")
        self.assertEqual(packages["omlx"], "0.5.3")
        self.assertEqual(packages["uv"], "0.11.10")
        self.assertEqual(
            packages["mlx_lm_revision"],
            "ab1806e8f5d6aa035973af194a1b9198ab4754dc",
        )
        self.assertEqual(packages["xgrammar"], "0.2.3")
        self.assertEqual(packages["apache_tvm_ffi"], "0.1.11")
        self.assertEqual(packages["minimum_macos"], "14.0")

    def test_component_paths_are_relative_and_cannot_escape(self) -> None:
        for component in self.manifest["components"].values():
            path = PurePosixPath(component["path"])
            self.assertFalse(path.is_absolute())
            self.assertNotIn("..", path.parts)
            self.assertNotEqual(str(path), ".")

    def test_tauri_bundle_includes_the_manifest(self) -> None:
        config = json.loads(TAURI_CONFIG.read_text(encoding="utf-8"))
        resources = config["bundle"]["resources"]
        self.assertIn("resources/runtime-manifest.json", resources)


if __name__ == "__main__":
    unittest.main()
