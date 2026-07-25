"""Deterministic, dependency-free report writers for M0a."""

from __future__ import annotations

import csv
from html import escape
from pathlib import Path
from typing import Mapping, Sequence


HIT_RATE_FIELDS = (
    "policy",
    "precision",
    "budget_bytes",
    "split",
    "capacity_per_layer",
    "pin_fraction",
    "hits",
    "accesses",
    "hit_rate",
    "all_hit_events",
    "layer_token_events",
    "all_hit_rate",
)
CONVERGENCE_FIELDS = (
    "decode_tokens",
    "calibration_tokens",
    "held_out_tokens",
    "hit_rate",
)
COUPLING_FIELDS = (
    "row_type",
    "depth",
    "budget",
    "observed_pairs",
    "lift_median",
    "lift_p90",
    "lift_p99",
    "frechet_above_50_fraction",
    "frechet_above_90_fraction",
    "marginal_hits",
    "coupled_hits",
    "accesses",
    "marginal_recall",
    "coupled_recall",
    "coupled_gain_pp",
)


def _write_csv(
    path: Path,
    fieldnames: Sequence[str],
    rows: Sequence[Mapping[str, object]],
) -> None:
    with path.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(
            stream,
            fieldnames=fieldnames,
            extrasaction="ignore",
            lineterminator="\n",
        )
        writer.writeheader()
        for row in rows:
            writer.writerow(row)


def write_hit_rates_csv(path: Path, rows: Sequence[Mapping[str, object]]) -> None:
    _write_csv(path, HIT_RATE_FIELDS, rows)


def write_convergence_csv(path: Path, rows: Sequence[Mapping[str, object]]) -> None:
    _write_csv(path, CONVERGENCE_FIELDS, rows)


def write_coupling_csv(path: Path, rows: Sequence[Mapping[str, object]]) -> None:
    _write_csv(path, COUPLING_FIELDS, rows)


def write_hit_rates_svg(path: Path, rows: Sequence[Mapping[str, object]]) -> None:
    width, height = 800, 480
    left, right, top, bottom = 80, 30, 45, 70
    plot_width = width - left - right
    plot_height = height - top - bottom
    budgets = sorted({int(row["budget_bytes"]) for row in rows})
    primary_rows = [row for row in rows if row["precision"] == "int4"]
    policies = sorted({str(row["policy"]) for row in primary_rows})
    colors = ("#0057b8", "#d1495b", "#2a9d8f", "#7b2cbf", "#e76f51")

    def x_for(budget: int) -> float:
        if len(budgets) <= 1:
            return left + plot_width / 2
        return left + budgets.index(budget) * plot_width / (len(budgets) - 1)

    def y_for(rate: float) -> float:
        return top + (1.0 - max(0.0, min(1.0, rate))) * plot_height

    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="white"/>',
        '<text x="400" y="26" text-anchor="middle" font-family="sans-serif" font-size="18">M0a held-out expert-access hit rate</text>',
    ]
    for tick in range(0, 11, 2):
        rate = tick / 10
        y = y_for(rate)
        lines.append(
            f'<line x1="{left}" y1="{y:.2f}" x2="{width-right}" y2="{y:.2f}" stroke="#dddddd"/>'
        )
        lines.append(
            f'<text x="{left-10}" y="{y+4:.2f}" text-anchor="end" font-family="sans-serif" font-size="11">{rate:.1f}</text>'
        )
    for budget in budgets:
        x = x_for(budget)
        lines.append(
            f'<text x="{x:.2f}" y="{height-bottom+24}" text-anchor="middle" font-family="sans-serif" font-size="11">{budget//1_000_000_000} GB</text>'
        )

    for index, policy in enumerate(policies):
        color = colors[index % len(colors)]
        policy_rows = sorted(
            (row for row in primary_rows if row["policy"] == policy),
            key=lambda row: int(row["budget_bytes"]),
        )
        points = " ".join(
            f'{x_for(int(row["budget_bytes"])):.2f},{y_for(float(row["hit_rate"])):.2f}'
            for row in policy_rows
        )
        lines.append(
            f'<polyline points="{points}" fill="none" stroke="{color}" stroke-width="2"/>'
        )
        legend_y = top + 16 * index
        lines.append(
            f'<line x1="{left+8}" y1="{legend_y}" x2="{left+28}" y2="{legend_y}" stroke="{color}" stroke-width="2"/>'
        )
        lines.append(
            f'<text x="{left+34}" y="{legend_y+4}" font-family="sans-serif" font-size="11">{escape(policy)}</text>'
        )

    lines.extend(
        [
            f'<line x1="{left}" y1="{top}" x2="{left}" y2="{height-bottom}" stroke="black"/>',
            f'<line x1="{left}" y1="{height-bottom}" x2="{width-right}" y2="{height-bottom}" stroke="black"/>',
            "</svg>",
        ]
    )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
