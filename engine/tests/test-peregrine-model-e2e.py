#!/usr/bin/env python3
"""Generate a real llama.cpp MoE fixture and prove resident/PGRN decode parity."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def run(command: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, cwd=cwd, text=True, capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--generator", required=True, type=Path)
    parser.add_argument("--driver", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="pgr_model_e2e_") as tmp:
        tmp_path = Path(tmp)
        env = os.environ.copy()
        env["PYTHONPATH"] = str(args.repo_root)
        proofs: list[str] = []
        artifacts: dict[str, tuple[Path, Path]] = {}
        for arch in ("qwen3moe", "qwen35moe"):
            run([str(args.generator), "-a", arch, "-s", "42", "-o", str(tmp_path)], cwd=args.repo_root)
            gguf = tmp_path / f"{arch}-moe.gguf"
            if not gguf.is_file():
                raise RuntimeError(f"llama architecture fixture did not create {gguf.name}")
            sha = hashlib.sha256(gguf.read_bytes()).hexdigest()
            pgrn = tmp_path / f"{arch}-moe.pgrn"
            converted = subprocess.run(
                [sys.executable, "-m", "bench.m1.convert_gguf_to_pgrn",
                 "--input", str(gguf), "--output", str(pgrn), "--model-sha256", sha],
                cwd=args.repo_root, env=env, text=True, capture_output=True, check=False,
            )
            if converted.returncode:
                raise RuntimeError(f"conversion failed for {arch}:\n{converted.stdout}\n{converted.stderr}")
            parity = run([str(args.driver), str(gguf), str(pgrn)], cwd=args.repo_root)
            if "PGR_MODEL_PARITY_OK" not in parity.stdout or "nmse=0" not in parity.stdout:
                raise RuntimeError(f"decoder parity proof missing for {arch}:\n{parity.stdout}\n{parity.stderr}")
            proofs.append(parity.stdout.strip())
            metal_parity = run(
                [str(args.driver), str(gguf), str(pgrn), "--gpu"], cwd=args.repo_root
            )
            if (
                "PGR_MODEL_PARITY_OK" not in metal_parity.stdout
                or "device=gpu" not in metal_parity.stdout
                or "nmse=0" not in metal_parity.stdout
            ):
                raise RuntimeError(
                    f"Metal decoder parity proof missing for {arch}:\n"
                    f"{metal_parity.stdout}\n{metal_parity.stderr}"
                )
            proofs.append(metal_parity.stdout.strip())
            coupling_parity = run(
                [str(args.driver), str(gguf), str(pgrn), "--coupling"], cwd=args.repo_root
            )
            if (
                "PGR_MODEL_PARITY_OK" not in coupling_parity.stdout
                or "coupling=yes" not in coupling_parity.stdout
                or "nmse=0" not in coupling_parity.stdout
            ):
                raise RuntimeError(
                    f"coupled-prefetch parity proof missing for {arch}:\n"
                    f"{coupling_parity.stdout}\n{coupling_parity.stderr}"
                )
            proofs.append(coupling_parity.stdout.strip())
            artifacts[arch] = (gguf, pgrn)

        q35_gguf, q35_pgrn = artifacts["qwen35moe"]
        for compact_args, expected_device in ((["--compact"], "default"), (["--compact", "--gpu"], "gpu")):
            compact_parity = run(
                [str(args.driver), str(q35_gguf), str(q35_pgrn), *compact_args], cwd=args.repo_root
            )
            if (
                "PGR_MODEL_PARITY_OK" not in compact_parity.stdout
                or "compact=yes" not in compact_parity.stdout
                or f"device={expected_device}" not in compact_parity.stdout
                or "nmse=0" not in compact_parity.stdout
            ):
                raise RuntimeError(
                    f"compact decoder parity proof missing:\n{compact_parity.stdout}\n{compact_parity.stderr}"
                )
            proofs.append(compact_parity.stdout.strip())

        mtp_parity = run([str(args.driver), str(q35_gguf), str(q35_pgrn), "--mtp"], cwd=args.repo_root)
        if "mode=mtp" not in mtp_parity.stdout or "nmse=0" not in mtp_parity.stdout:
            raise RuntimeError(f"MTP parity proof missing:\n{mtp_parity.stdout}\n{mtp_parity.stderr}")
        proofs.append(mtp_parity.stdout.strip())
        mtp_metal_parity = run(
            [str(args.driver), str(q35_gguf), str(q35_pgrn), "--mtp", "--gpu"],
            cwd=args.repo_root,
        )
        if (
            "mode=mtp" not in mtp_metal_parity.stdout
            or "device=gpu" not in mtp_metal_parity.stdout
            or "nmse=0" not in mtp_metal_parity.stdout
        ):
            raise RuntimeError(
                f"Metal MTP parity proof missing:\n"
                f"{mtp_metal_parity.stdout}\n{mtp_metal_parity.stderr}"
            )
        proofs.append(mtp_metal_parity.stdout.strip())

        for compact_args, expected_device in (
            (["--mtp", "--compact"], "default"),
            (["--mtp", "--compact", "--gpu"], "gpu"),
        ):
            compact_mtp = run(
                [str(args.driver), str(q35_gguf), str(q35_pgrn), *compact_args], cwd=args.repo_root
            )
            if (
                "mode=mtp" not in compact_mtp.stdout
                or "compact=yes" not in compact_mtp.stdout
                or f"device={expected_device}" not in compact_mtp.stdout
                or "nmse=0" not in compact_mtp.stdout
            ):
                raise RuntimeError(
                    f"compact MTP parity proof missing:\n{compact_mtp.stdout}\n{compact_mtp.stderr}"
                )
            proofs.append(compact_mtp.stdout.strip())

        gguf, pgrn = artifacts["qwen3moe"]
        wrong_gguf = tmp_path / "qwen3moe-wrong-identity.gguf"
        shutil.copyfile(gguf, wrong_gguf)
        with wrong_gguf.open("r+b") as stream:
            stream.seek(-1, os.SEEK_END)
            original = stream.read(1)
            stream.seek(-1, os.SEEK_END)
            stream.write(bytes([original[0] ^ 1]))
        rejected = subprocess.run(
            [str(args.driver), str(wrong_gguf), str(pgrn)],
            cwd=args.repo_root, text=True, capture_output=True, check=False,
        )
        rejection_output = rejected.stdout + rejected.stderr
        if rejected.returncode == 0 or "model identity does not match" not in rejection_output:
            raise RuntimeError(
                "wrong GGUF identity was not rejected by the native loader:\n"
                f"{rejected.stdout}\n{rejected.stderr}"
            )
        print("\n".join(proofs))
        print("PGR_MODEL_IDENTITY_REJECTION_OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
