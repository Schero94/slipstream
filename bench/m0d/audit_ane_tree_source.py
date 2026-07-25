"""Fail-closed audit for a real one-shot/tree draft source.

Sequential MTP/nextn blocks are deliberately not accepted as tree heads. A source is
eligible only when it carries an explicit tree-head contract or a supported named
Medusa head family. This prevents relabeling ordinary autoregressive draft weights as
an ANE one-shot model.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any, Mapping, Sequence

from bench.m1.gguf_reader import read_gguf_file


def audit_header(header: Mapping[str, Any]) -> dict[str, Any]:
    metadata = header.get("metadata")
    tensors = header.get("tensors")
    if not isinstance(metadata, Mapping) or not isinstance(tensors, Sequence):
        raise ValueError("GGUF header is incomplete")
    relevant_metadata = {
        str(key): value
        for key, value in metadata.items()
        if any(term in str(key).lower() for term in ("nextn", "mtp", "medusa", "eagle", "tree"))
    }
    names = [str(getattr(tensor, "name", "")) for tensor in tensors]
    relevant_tensors = [
        name for name in names
        if any(term in name.lower() for term in ("nextn", "mtp", "medusa", "eagle", "tree"))
    ]
    explicit_heads = metadata.get("peregrine.ane_tree_heads")
    medusa_tensors = [name for name in relevant_tensors if "medusa" in name.lower()]
    eligible = type(explicit_heads) is int and explicit_heads > 1 and bool(medusa_tensors)
    nextn_layers = next(
        (value for key, value in relevant_metadata.items() if key.endswith(".nextn_predict_layers")),
        None,
    )
    if eligible:
        reason = "explicit supported one-shot tree heads"
    elif nextn_layers:
        reason = "sequential MTP/nextn weights are present, but no supported one-shot tree heads"
    else:
        reason = "no supported one-shot tree heads are present"
    return {
        "eligible": eligible,
        "reason": reason,
        "nextn_predict_layers": nextn_layers,
        "relevant_metadata": relevant_metadata,
        "relevant_tensors": relevant_tensors,
    }


def audit_file(path: Path) -> dict[str, Any]:
    result = audit_header(read_gguf_file(path))
    return {"source": str(path), **result}


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("gguf", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(argv)
    report = audit_file(args.gguf)
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    print(encoded, end="")
    return 0 if report["eligible"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
