"""Audit S-dependent Metal kernel selection during embedded MTP speculation."""

from __future__ import annotations

import argparse
from dataclasses import asdict
import json
from pathlib import Path
import re
import subprocess
from typing import Iterable, Mapping

from bench.m0a.qualify_model import (
    QualificationProfile,
    _read_manifest,
    production_environment,
    qualification_server_command,
)
from bench.m0a.smoke_server import (
    DEFAULT_SERVER,
    SmokeError,
    _json_request,
    _unused_port,
    _wait_for_health,
    _write_json_atomic,
)


GENERAL_SMALL_BATCH_TYPES = frozenset(
    {
        "F32",
        "F16",
        "BF16",
        "Q1_0",
        "Q2_0",
        "Q4_0",
        "Q4_1",
        "Q5_0",
        "Q5_1",
        "Q8_0",
        "MXFP4",
        "IQ4_NL",
    }
)
K_SMALL_BATCH_TYPES = frozenset({"Q4_K", "Q5_K", "Q6_K", "Q2_K", "Q3_K"})
TRACE_PROFILE = QualificationProfile(
    name="spec-pin-audit-f16-fa-mtp8",
    flash_attention="on",
    cache_type_k="f16",
    cache_type_v="f16",
    speculation="draft-mtp",
    draft_tokens=8,
)
_NODE = re.compile(r"node\[\s*\d+\]\s+-\s+(MUL_MAT(?:_ID)?)\b")
_SOURCE = re.compile(
    r"src([012])\s+-\s+(\S+)\s+"
    r"\[\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+)\]"
    r".*?,\s*[01],\s*(.+?)\s*$"
)
_SPEC_PIN_EVENT = re.compile(
    r"\[SPEC_PIN\]\s+Metal dense MUL_MAT rows=(\d+) forced to S=1 mul_mv family"
)


class SpecPinAuditError(RuntimeError):
    """Raised when a kernel trace cannot support the S2 decision."""


def select_metal_kernel_family(
    weight_type: str,
    rows: int,
    *,
    op: str = "MUL_MAT",
    has_simdgroup_mm: bool = True,
    inner_dimension: int = 128,
    spec_pinned: bool = False,
) -> str:
    """Mirror the fork's current Metal branch thresholds for audit reports."""

    normalized = weight_type.upper()
    if op == "MUL_MAT_ID":
        if has_simdgroup_mm and inner_dimension >= 64 and rows >= 32:
            return "mul_mm_id"
        return "mul_mv_id"
    if op != "MUL_MAT":
        raise ValueError(f"unsupported matrix operation: {op}")
    if spec_pinned:
        return "mul_mv"
    if inner_dimension % 128 == 0:
        if normalized in GENERAL_SMALL_BATCH_TYPES and 2 <= rows <= 8:
            return "mul_mv_ext"
        if normalized in K_SMALL_BATCH_TYPES and 4 <= rows <= 8:
            return "mul_mv_ext"
    if has_simdgroup_mm and inner_dimension >= 64 and rows > 8:
        return "mul_mm"
    return "mul_mv"


def parse_metal_graph_trace(text: str) -> list[dict[str, object]]:
    """Extract matrix op identity and row count from GRAPH_DEBUG=2 logs."""

    records: list[dict[str, object]] = []
    current: dict[str, object] | None = None

    def finish() -> None:
        nonlocal current
        if current is None or "src0" not in current:
            current = None
            return
        required_row_source = "src2" if current["op"] == "MUL_MAT_ID" else "src1"
        source = current.get(required_row_source)
        weight = current.get("src0")
        if isinstance(source, Mapping) and isinstance(weight, Mapping):
            rows = int(source["dimensions"][1])
            weight_type = str(weight["type"])
            inner_dimension = int(weight["dimensions"][0])
            records.append(
                {
                    "op": current["op"],
                    "tensor": weight["name"],
                    "weight_type": weight_type,
                    "rows": rows,
                    "inner_dimension": inner_dimension,
                    "kernel_family": select_metal_kernel_family(
                        weight_type,
                        rows,
                        op=str(current["op"]),
                        inner_dimension=inner_dimension,
                    ),
                }
            )
        current = None

    for line in text.splitlines():
        node = _NODE.search(line)
        if node:
            finish()
            current = {"op": node.group(1)}
            continue
        if current is None:
            continue
        source = _SOURCE.search(line)
        if source:
            current[f"src{source.group(1)}"] = {
                "type": source.group(2).upper(),
                "dimensions": tuple(int(source.group(index)) for index in range(3, 7)),
                "name": source.group(7),
            }
            if current["op"] == "MUL_MAT" and source.group(1) == "1":
                finish()
            elif current["op"] == "MUL_MAT_ID" and source.group(1) == "2":
                finish()
    finish()
    return records


def parse_spec_pin_events(text: str) -> list[int]:
    return sorted({int(match) for match in _SPEC_PIN_EVENT.findall(text)})


def audit_kernel_trace(
    records: Iterable[Mapping[str, object]], *, max_verify_rows: int = 13
) -> dict[str, object]:
    records = list(records)
    dense = [record for record in records if record.get("op") == "MUL_MAT"]
    moe = [record for record in records if record.get("op") == "MUL_MAT_ID"]
    has_draft = any(record.get("rows") == 1 for record in dense)
    has_verify = any(
        isinstance(record.get("rows"), int) and 2 <= int(record["rows"]) <= max_verify_rows
        for record in dense
    )
    reasons: list[str] = []
    if not has_draft:
        reasons.append("no-draft-row")
    if not has_verify:
        reasons.append("no-verify-row")

    def divergences_for(group: list[Mapping[str, object]]) -> list[dict[str, object]]:
        by_tensor: dict[tuple[object, object], list[Mapping[str, object]]] = {}
        for record in group:
            by_tensor.setdefault((record.get("tensor"), record.get("weight_type")), []).append(record)
        divergences: list[dict[str, object]] = []
        for (tensor, weight_type), items in sorted(by_tensor.items(), key=lambda item: str(item[0])):
            draft = {str(item["kernel_family"]) for item in items if item.get("rows") == 1}
            verify = {
                str(item["kernel_family"])
                for item in items
                if isinstance(item.get("rows"), int)
                and 2 <= int(item["rows"]) <= max_verify_rows
            }
            if draft and verify and draft != verify:
                divergences.append(
                    {
                        "tensor": tensor,
                        "weight_type": weight_type,
                        "draft_rows": sorted({int(item["rows"]) for item in items if item.get("rows") == 1}),
                        "verify_rows": sorted(
                            {
                                int(item["rows"])
                                for item in items
                                if isinstance(item.get("rows"), int)
                                and 2 <= int(item["rows"]) <= max_verify_rows
                            }
                        ),
                        "draft_families": sorted(draft),
                        "verify_families": sorted(verify),
                    }
                )
        return divergences

    dense_divergences = divergences_for(dense)
    moe_divergences = divergences_for(moe)
    return {
        "complete": not reasons,
        "reasons": reasons,
        "record_count": len(records),
        "dense_record_count": len(dense),
        "moe_record_count": len(moe),
        "dense_divergence_observed": bool(dense_divergences),
        "moe_divergence_observed": bool(moe_divergences),
        "divergence_count": len(dense_divergences) + len(moe_divergences),
        "divergences": dense_divergences + moe_divergences,
    }


def run_kernel_trace(
    model: Path,
    output_dir: Path,
    *,
    server: Path = DEFAULT_SERVER,
    spec_pin: bool = False,
) -> dict[str, object]:
    manifest = _read_manifest(model)
    output_dir.mkdir(parents=True, exist_ok=False)
    port = _unused_port()
    command = qualification_server_command(
        model,
        port,
        server=server,
        profile=TRACE_PROFILE,
    )
    command.append("--verbose")
    environment = production_environment()
    environment["GGML_METAL_GRAPH_DEBUG"] = "2"
    if spec_pin:
        environment["SPEC_PIN"] = "1"
    else:
        environment.pop("SPEC_PIN", None)
    stderr_path = output_dir / "metal-graph-debug.log"
    with (output_dir / "server.stdout.log").open("wb") as stdout, stderr_path.open(
        "wb"
    ) as stderr:
        process = subprocess.Popen(
            command,
            env=environment,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            _wait_for_health(process, port)
            completion = _json_request(
                f"http://127.0.0.1:{port}/completion",
                {
                    "prompt": "Implement a deterministic bounded retry helper in Python.",
                    "n_predict": 16,
                    "ignore_eos": True,
                    "temperature": 0,
                    "seed": 42,
                    "stream": False,
                    "cache_prompt": False,
                },
                timeout=900,
            )
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
    trace = stderr_path.read_text(encoding="utf-8", errors="replace")
    records = parse_metal_graph_trace(trace)
    spec_pin_events = parse_spec_pin_events(trace)
    audit = audit_kernel_trace(records)
    report: dict[str, object] = {
        "schema": 1,
        "model_sha256": manifest["sha256"],
        "runtime_profile": asdict(TRACE_PROFILE),
        "command": command,
        "trace_path": str(stderr_path.resolve()),
        "trace_bytes": stderr_path.stat().st_size,
        "completion_timings": completion.get("timings"),
        "spec_pin_enabled": spec_pin,
        "spec_pin_event_rows": spec_pin_events,
        "audit": audit,
        "records": records,
    }
    _write_json_atomic(output_dir / "spec-pin-audit.json", report)
    if not audit["complete"]:
        raise SpecPinAuditError(f"kernel trace is incomplete: {audit['reasons']}")
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument("--spec-pin", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        report = run_kernel_trace(
            args.model,
            args.output_dir,
            server=args.server,
            spec_pin=args.spec_pin,
        )
    except (OSError, SmokeError, SpecPinAuditError, ValueError) as error:
        print(f"SPEC_PIN audit failed: {error}")
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
