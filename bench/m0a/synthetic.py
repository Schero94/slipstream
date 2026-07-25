"""Generate a deterministic, complete M0a routing session."""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Iterator
from uuid import UUID

from bench.m0a.constants import DEV_EXPERTS, DEV_LAYERS, DEV_TOP_K
from bench.m0a.routing_format import (
    PHASE_DECODE,
    RoutingHeader,
    RoutingRecord,
    write_fixture,
)


SYNTHETIC_SESSION_ID = UUID("00000000-0000-0000-0000-000000000001")


def records(decode_tokens: int) -> Iterator[RoutingRecord]:
    for token_pos in range(decode_tokens):
        for layer in range(DEV_LAYERS):
            yield RoutingRecord(
                session_id=SYNTHETIC_SESSION_ID,
                batch_id=token_pos + 1,
                token_pos=token_pos,
                token_id=token_pos % 248_320,
                sequence_id=0,
                phase=PHASE_DECODE,
                layer=layer,
                experts=tuple(
                    (token_pos + layer + rank) % DEV_EXPERTS
                    for rank in range(DEV_TOP_K)
                ),
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--decode-tokens", required=True, type=int)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.decode_tokens < 0:
        raise SystemExit("--decode-tokens must be non-negative")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    path = args.output_dir / "routing-synthetic.bin"
    header = RoutingHeader(
        layer_count=DEV_LAYERS,
        expert_count=DEV_EXPERTS,
        top_k=DEV_TOP_K,
        flags=0,
        session_id=SYNTHETIC_SESSION_ID,
        start_time_ns=0,
        model_sha256=b"\x00" * 32,
    )
    write_fixture(path, header, records(args.decode_tokens))
    print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
