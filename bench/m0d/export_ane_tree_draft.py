"""Create an identity-bound manifest for an already trained Core ML draft.

This tool never invents or converts weights. It requires a prior source audit that
explicitly proves compatible one-shot/tree heads and a compiled .mlmodelc package.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import struct
import tempfile
from typing import Any, Mapping, Sequence


FORMAT_TAG = b"PGRANE1\0"


def sha256_file(path: Path, chunk_bytes: int = 1024 * 1024) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(chunk_bytes):
            digest.update(chunk)
    return digest.hexdigest()


def package_sha256(package: Path) -> str:
    if not package.is_dir() or package.suffix != ".mlmodelc":
        raise ValueError("compiled package must be an existing .mlmodelc directory")
    files: list[tuple[str, Path]] = []
    for entry in package.rglob("*"):
        if entry.is_symlink():
            raise ValueError("compiled package contains a symlink")
        if entry.is_file():
            files.append((entry.relative_to(package).as_posix(), entry))
        elif not entry.is_dir():
            raise ValueError("compiled package contains a non-regular entry")
    files.sort(key=lambda item: item[0])
    digest = hashlib.sha256(FORMAT_TAG)
    for relative, path in files:
        encoded = relative.encode("utf-8")
        size = path.stat().st_size
        digest.update(struct.pack("<Q", len(encoded)))
        digest.update(encoded)
        digest.update(struct.pack("<Q", size))
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    return digest.hexdigest()


def build_manifest(
    *,
    source_gguf: Path,
    compiled_model: Path,
    output: Path,
    audit: Mapping[str, Any],
    architecture: str,
    input_name: str,
    input_count: int,
    pad_token: int,
    output_name: str,
    depth: int,
    vocabulary: int,
    precision: str,
) -> dict[str, Any]:
    if audit.get("eligible") is not True:
        raise ValueError("source audit does not prove compatible one-shot/tree weights")
    if not source_gguf.is_file() or source_gguf.suffix.lower() != ".gguf":
        raise ValueError("source GGUF is absent or invalid")
    if type(input_count) is not int or input_count <= 0:
        raise ValueError("input count must be a positive integer")
    if type(depth) is not int or depth <= 0:
        raise ValueError("draft depth must be a positive integer")
    if type(vocabulary) is not int or vocabulary <= 0:
        raise ValueError("vocabulary must be a positive integer")
    if type(pad_token) is not int or not -(2**31) <= pad_token < 2**31:
        raise ValueError("pad token must fit Int32")
    if not all(isinstance(value, str) and value for value in
               (architecture, input_name, output_name, precision)):
        raise ValueError("manifest strings cannot be empty")
    output_parent = output.resolve().parent
    package_resolved = compiled_model.resolve()
    try:
        package_relative = package_resolved.relative_to(output_parent).as_posix()
    except ValueError as error:
        raise ValueError("compiled package must be inside the manifest directory") from error
    return {
        "schema": "peregrine-ane-v1",
        "mode": "one-shot-linear",
        "compiled_model": package_relative,
        "package_sha256": package_sha256(package_resolved),
        "source_model_sha256": sha256_file(source_gguf),
        "architecture": architecture,
        "input": {"name": input_name, "count": input_count, "pad_token": pad_token},
        "output": {"name": output_name, "count": depth},
        "draft": {"depth": depth, "width": 1, "vocabulary": vocabulary, "precision": precision},
    }


def write_manifest(output: Path, manifest: Mapping[str, Any]) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(manifest, indent=2, sort_keys=False) + "\n"
    with tempfile.NamedTemporaryFile("w", dir=output.parent, delete=False) as temporary:
        temporary.write(encoded)
        temporary.flush()
        temporary_path = Path(temporary.name)
    temporary_path.replace(output)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-gguf", type=Path, required=True)
    parser.add_argument("--compiled-model", type=Path, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--architecture", required=True)
    parser.add_argument("--input-name", default="tokens")
    parser.add_argument("--input-count", type=int, required=True)
    parser.add_argument("--pad-token", type=int, required=True)
    parser.add_argument("--output-name", default="candidates")
    parser.add_argument("--depth", type=int, required=True)
    parser.add_argument("--vocabulary", type=int, required=True)
    parser.add_argument("--precision", required=True)
    args = parser.parse_args(argv)
    audit = json.loads(args.audit.read_text())
    manifest = build_manifest(
        source_gguf=args.source_gguf,
        compiled_model=args.compiled_model,
        output=args.output,
        audit=audit,
        architecture=args.architecture,
        input_name=args.input_name,
        input_count=args.input_count,
        pad_token=args.pad_token,
        output_name=args.output_name,
        depth=args.depth,
        vocabulary=args.vocabulary,
        precision=args.precision,
    )
    write_manifest(args.output, manifest)
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
