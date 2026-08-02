import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from bench.vendor.run_candidate_ab import (
    EvidenceError,
    evaluate,
    git_clean_head,
    run_side,
    validate_candidate,
)


def metric(output="same", tok_s=10.0, ttft_ms=100.0, rss_mb=1000.0, swap_mb=0.0):
    return {
        "output": output,
        "tok_s": tok_s,
        "ttft_ms": ttft_ms,
        "peak_rss_mb": rss_mb,
        "swap_delta_mb": swap_mb,
    }


class CandidateAbTests(unittest.TestCase):
    def test_audited_manifest_is_pinned_unique_and_honestly_unqualified(self):
        manifest = json.loads(Path("bench/vendor/candidates.json").read_text())
        candidates = manifest["candidates"]
        self.assertEqual(len(candidates), 15)
        self.assertEqual(len({item["id"] for item in candidates}), len(candidates))
        for item in candidates:
            self.assertRegex(item["baseline_sha"], r"^[0-9a-f]{40}$")
            self.assertRegex(item["candidate_sha"], r"^[0-9a-f]{40}$")
            self.assertEqual(item["status"], "unqualified")
            self.assertEqual(item["command"], [])
            self.assertTrue(item["url"].startswith("https://github.com/"))

    def test_clean_worktree_and_pinned_head_are_required(self):
        with tempfile.TemporaryDirectory() as raw:
            repo = Path(raw)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "user.email", "ab@test"], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "user.name", "AB"], check=True)
            (repo / "tracked").write_text("ok\n")
            subprocess.run(["git", "-C", str(repo), "add", "tracked"], check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-qm", "base"], check=True)
            head = git_clean_head(repo)
            self.assertEqual(len(head), 40)
            (repo / "tracked").write_text("dirty\n")
            with self.assertRaises(EvidenceError):
                git_clean_head(repo)

    def test_candidate_requires_full_pins_and_command(self):
        candidate = {
            "id": "vendor-pr",
            "status": "unqualified",
            "baseline_sha": "a" * 40,
            "candidate_sha": "b" * 40,
            "command": ["tool", "--json"],
            "acceptance": {},
        }
        validate_candidate(candidate)
        for key in ("baseline_sha", "candidate_sha", "command"):
            broken = dict(candidate)
            broken.pop(key)
            with self.assertRaises(EvidenceError, msg=key):
                validate_candidate(broken)

    def test_accepts_deterministic_quality_and_measured_speedup(self):
        baseline = [metric(tok_s=v) for v in (10.0, 10.2, 9.8)]
        candidate = [metric(tok_s=v, ttft_ms=95.0) for v in (11.0, 11.2, 10.8)]
        report = evaluate(
            baseline,
            candidate,
            {"min_tok_s_ratio": 1.05, "max_ttft_ratio": 1.10, "max_rss_ratio": 1.10},
        )
        self.assertEqual(report["decision"], "accepted")
        self.assertGreater(report["ratios"]["tok_s"], 1.05)

    def test_rejects_quality_change_missing_evidence_or_swap(self):
        baseline = [metric(), metric(), metric()]
        changed = [metric(output="different") for _ in range(3)]
        self.assertEqual(evaluate(baseline, changed, {})["decision"], "rejected")
        swapped = [metric(swap_mb=1.0) for _ in range(3)]
        self.assertEqual(evaluate(baseline, swapped, {})["decision"], "rejected")
        incomplete = metric()
        incomplete.pop("peak_rss_mb")
        with self.assertRaises(EvidenceError):
            evaluate(baseline, [incomplete] * 3, {})

    def test_runner_excludes_warmups_and_keeps_requested_repeats(self):
        with tempfile.TemporaryDirectory() as raw:
            worktree = Path(raw)
            payload = json.dumps(metric())
            command = [sys.executable, "-c", f"print({payload!r})"]
            runs = run_side(command, worktree, "baseline", warmups=1, repeats=3)
            self.assertEqual(len(runs), 3)
            self.assertTrue(all(len(run["output_sha256"]) == 64 for run in runs))
            self.assertNotIn("output", runs[0])


if __name__ == "__main__":
    unittest.main()
