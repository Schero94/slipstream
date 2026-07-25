"""Local-only trigger, redaction, briefing, and ledger core for Track E."""

from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
from typing import Iterable, Mapping


HEX64 = re.compile(r"^[0-9a-f]{64}$")
MODES = {"ask", "auto", "never"}
TOKEN_BUDGET = 8_000
PROTECTED_COMPONENTS = {"hidden_tests", ".heldout", "heldout"}
SUCCESS_CLAIMS = re.compile(
    r"\b(all tests pass(?:ed)?|tests pass(?:ed)?|verified successfully|verification pass(?:ed)?)\b",
    re.IGNORECASE,
)
FILE_CLAIM = re.compile(
    r"(?<![A-Za-z0-9_.-])(?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.[A-Za-z0-9_]+"
)
BENCHMARK_CODE = re.compile(
    r"(?:perf_counter|timeit|pytest[_-]?benchmark|benchmark\s*\(|"
    r"tokens?_per_second|tok/s|latency|throughput|large[_-]?fixture)",
    re.IGNORECASE,
)
SENSITIVE_ASSIGNMENT = re.compile(
    r"(?im)^(?P<prefix>\s*(?:export\s+)?[A-Za-z_][A-Za-z0-9_]*"
    r"(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL)[A-Za-z0-9_]*\s*[:=]\s*)"
    r"(?P<value>[^\r\n]+)$"
)
SENSITIVE_MAPPING = re.compile(
    r"(?im)^(?P<prefix>\s*(?:api[_-]?key|token|secret|password|passwd|credential)"
    r"\s*:\s*)(?P<value>[^\r\n]+)$"
)
BEARER = re.compile(r"(?i)(Bearer\s+)[A-Za-z0-9._~+/=-]+")
PRIVATE_KEY = re.compile(
    r"-----BEGIN [^-\r\n]*PRIVATE KEY-----.*?-----END [^-\r\n]*PRIVATE KEY-----",
    re.DOTALL,
)


class EscalationError(RuntimeError):
    pass


@dataclass(frozen=True)
class ForbiddenRule:
    rule_id: str
    pattern: str
    regex: bool = False

    def __post_init__(self) -> None:
        if not re.fullmatch(r"[a-z0-9][a-z0-9_.-]*", self.rule_id):
            raise EscalationError("forbidden rule ID is unsafe")
        if not self.pattern:
            raise EscalationError("forbidden rule pattern is empty")
        if self.regex:
            try:
                re.compile(self.pattern)
            except re.error as error:
                raise EscalationError(f"invalid forbidden regex: {error}") from error


@dataclass(frozen=True)
class Finding:
    detector_id: str
    severity: str
    evidence: str
    negative_constraint: str


@dataclass(frozen=True)
class TriggerContext:
    task_contract: str
    local_diff: str
    summary: str
    verifier_passed: bool
    verifier_output: str
    verifier_command: tuple[str, ...]
    accessed_paths: tuple[str, ...]
    file_pointers: tuple[str, ...]
    feedback_retries: int
    local_output_tokens: int
    manual_boost: bool

    def __post_init__(self) -> None:
        if not self.task_contract:
            raise EscalationError("task contract is empty")
        if not self.verifier_command or not all(self.verifier_command):
            raise EscalationError("verifier command is empty")
        for value, label in (
            (self.feedback_retries, "feedback retries"),
            (self.local_output_tokens, "local output tokens"),
        ):
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise EscalationError(f"{label} must be a non-negative integer")


@dataclass(frozen=True)
class EscalationDecision:
    mode: str
    action: str
    triggers: tuple[str, ...]
    provider_invoked: bool = False


@dataclass(frozen=True)
class Briefing:
    markdown: str
    sha256: str
    include_failed_diff: bool
    provider_state: str = "not_invoked"
    m0a_admitted_tokens: int = 0


def _diff_added_lines(diff: str) -> dict[str, list[tuple[int, str]]]:
    files: dict[str, list[tuple[int, str]]] = {}
    current: str | None = None
    new_line = 0
    for line in diff.splitlines():
        if line.startswith("+++ "):
            value = line[4:].strip()
            if value == "/dev/null":
                current = None
            else:
                current = value[2:] if value.startswith("b/") else value
                files.setdefault(current, [])
            continue
        if line.startswith("@@"):
            match = re.search(r"\+(\d+)", line)
            new_line = int(match.group(1)) if match else 0
            continue
        if current is None:
            continue
        if line.startswith("+") and not line.startswith("+++"):
            files[current].append((new_line, line[1:]))
            new_line += 1
        elif not line.startswith("-"):
            new_line += 1
    return files


def _safe_relative(path: str) -> str:
    normalized = path.replace("\\", "/")
    pure = PurePosixPath(normalized)
    if not normalized or pure.is_absolute() or ".." in pure.parts or "." in pure.parts:
        raise EscalationError(f"unsafe repository path: {path}")
    return str(pure)


def detect_violations(
    context: TriggerContext,
    *,
    forbidden_rules: Iterable[ForbiddenRule] = (),
    protected_roots: Iterable[str] = (),
) -> tuple[Finding, ...]:
    findings: list[Finding] = []
    added = _diff_added_lines(context.local_diff)
    for rule in forbidden_rules:
        matcher = re.compile(rule.pattern).search if rule.regex else None
        for path, lines in added.items():
            for line_number, line in lines:
                matched = bool(matcher(line)) if matcher else rule.pattern in line
                if matched:
                    findings.append(
                        Finding(
                            f"forbidden:{rule.rule_id}",
                            "error",
                            f"{path}:{line_number} matched forbidden rule {rule.rule_id}",
                            f"Do not introduce code matching forbidden rule {rule.rule_id}.",
                        )
                    )
                    break
            else:
                continue
            break

    protected = {_safe_relative(path) for path in protected_roots}
    for source in context.accessed_paths:
        path = _safe_relative(source)
        parts = set(PurePosixPath(path).parts)
        root_hit = any(path == root or path.startswith(root + "/") for root in protected)
        if parts & PROTECTED_COMPONENTS or root_hit:
            findings.append(
                Finding(
                    "held-out-leakage",
                    "error",
                    f"protected path accessed: {path}",
                    "Do not read or infer from held-out verifier artifacts.",
                )
            )

    changed = set(added)
    for claimed in sorted(set(FILE_CLAIM.findall(context.summary))):
        if claimed not in changed:
            findings.append(
                Finding(
                    "summary-vs-diff:file",
                    "error",
                    f"summary claims unchanged path: {claimed}",
                    "Only claim file changes that are present in the returned diff.",
                )
            )
    if not context.verifier_passed and SUCCESS_CLAIMS.search(context.summary):
        findings.append(
            Finding(
                "summary-vs-diff:verifier",
                "error",
                "summary claims success while verifier is red",
                "Do not claim success unless the exact acceptance command passes.",
            )
        )

    for path, lines in added.items():
        pure = PurePosixPath(path)
        is_test = "tests" in pure.parts or pure.name.startswith("test_")
        if is_test and any(BENCHMARK_CODE.search(line) for _, line in lines):
            findings.append(
                Finding(
                    "benchmark-in-unittest",
                    "error",
                    f"benchmark-like code added to unit test path: {path}",
                    "Keep runtime benchmarks and performance thresholds out of the unit suite.",
                )
            )
    return tuple(findings)


def decide_escalation(
    context: TriggerContext,
    *,
    mode: str,
    findings: Iterable[Finding],
) -> EscalationDecision:
    if mode not in MODES:
        raise EscalationError(f"unsupported escalation mode: {mode}")
    findings = tuple(findings)
    triggers: list[str] = []
    if not context.verifier_passed and context.feedback_retries >= 1:
        triggers.append("T1")
    if findings:
        triggers.append("T2")
    if context.local_output_tokens > TOKEN_BUDGET:
        triggers.append("T3")
    if context.manual_boost:
        triggers.append("T4")
    if mode == "never":
        return EscalationDecision(mode, "LOCAL_ONLY", tuple(triggers))
    if not triggers:
        action = "LOCAL_COMPLETE" if context.verifier_passed else "RETRY_WITH_FEEDBACK"
        return EscalationDecision(mode, action, ())
    return EscalationDecision(
        mode,
        "AWAITING_CONSENT" if mode == "ask" else "AUTO_ELIGIBLE",
        tuple(triggers),
    )


def redact(text: str, *, extra_patterns: Iterable[str] = ()) -> str:
    result = PRIVATE_KEY.sub("[REDACTED]", text)
    result = BEARER.sub(r"\1[REDACTED]", result)
    result = SENSITIVE_ASSIGNMENT.sub(r"\g<prefix>[REDACTED]", result)
    result = SENSITIVE_MAPPING.sub(r"\g<prefix>[REDACTED]", result)
    for pattern in extra_patterns:
        try:
            result = re.sub(pattern, "[REDACTED]", result)
        except re.error as error:
            raise EscalationError(f"invalid redaction regex: {error}") from error
    return result


def build_briefing(
    context: TriggerContext,
    findings: Iterable[Finding],
    *,
    include_failed_diff: bool,
    extra_redaction_patterns: Iterable[str] = (),
) -> Briefing:
    pointers = tuple(sorted({_safe_relative(path) for path in context.file_pointers}))
    findings = tuple(findings)
    constraints = [finding.negative_constraint for finding in findings]
    evidence = [finding.evidence for finding in findings]
    sections = [
        "# Peregrine Escalation Briefing",
        "",
        "## Task contract (verbatim)",
        "",
        redact(context.task_contract, extra_patterns=extra_redaction_patterns),
        "",
        "## Failure evidence",
        "",
        redact(context.verifier_output, extra_patterns=extra_redaction_patterns),
    ]
    if evidence:
        sections.extend(["", "## Detector findings", ""])
        sections.extend(f"- {redact(item, extra_patterns=extra_redaction_patterns)}" for item in evidence)
    if constraints:
        sections.extend(["", "## Negative constraints", ""])
        sections.extend(f"- {item}" for item in constraints)
    sections.extend(["", "## Repository file pointers", ""])
    sections.extend(f"- `{path}`" for path in pointers)
    sections.extend(
        [
            "",
            "## Exact acceptance command",
            "",
            "```text",
            shlex.join(context.verifier_command),
            "```",
            "",
            "## Return contract",
            "",
            "Return the patch plus `local_failure_cause` and `generalizable_lesson`.",
        ]
    )
    if include_failed_diff:
        sections.extend(
            [
                "",
                "## Failed local diff (failed attempt; a new approach is allowed)",
                "",
                "```diff",
                redact(context.local_diff, extra_patterns=extra_redaction_patterns),
                "```",
            ]
        )
    markdown = "\n".join(sections).rstrip() + "\n"
    digest = hashlib.sha256(markdown.encode("utf-8")).hexdigest()
    return Briefing(markdown, digest, include_failed_diff)


@dataclass(frozen=True)
class LedgerEntry:
    entry_id: str
    task_sha256: str
    local_evidence_sha256: str
    briefing_sha256: str
    triggers: tuple[str, ...]
    consent_state: str
    origin: str = "local-failure"
    provider_state: str = "not_invoked"
    verifier_outcome: str = "not_run"
    m0a_admitted_tokens: int = 0
    schema: int = 1

    @classmethod
    def create(
        cls,
        *,
        task_contract: str,
        local_evidence_sha256: str,
        briefing: Briefing,
        triggers: Iterable[str],
        consent_state: str,
    ) -> "LedgerEntry":
        if HEX64.fullmatch(local_evidence_sha256) is None:
            raise EscalationError("local evidence SHA-256 is invalid")
        if consent_state not in {"required", "granted", "denied", "not_applicable"}:
            raise EscalationError("consent state is invalid")
        task_hash = hashlib.sha256(task_contract.encode("utf-8")).hexdigest()
        trigger_tuple = tuple(triggers)
        identity = {
            "task_sha256": task_hash,
            "local_evidence_sha256": local_evidence_sha256,
            "briefing_sha256": briefing.sha256,
            "triggers": trigger_tuple,
            "consent_state": consent_state,
            "origin": "local-failure",
            "provider_state": "not_invoked",
            "verifier_outcome": "not_run",
            "m0a_admitted_tokens": 0,
            "schema": 1,
        }
        encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode("utf-8")
        return cls(hashlib.sha256(encoded).hexdigest(), **identity)

    def validate(self) -> None:
        if self.schema != 1:
            raise EscalationError("unsupported ledger schema")
        for value in (self.entry_id, self.task_sha256, self.local_evidence_sha256, self.briefing_sha256):
            if HEX64.fullmatch(value) is None:
                raise EscalationError("ledger hash is invalid")
        if (
            self.origin != "local-failure"
            or self.provider_state != "not_invoked"
            or self.verifier_outcome != "not_run"
            or self.m0a_admitted_tokens != 0
        ):
            raise EscalationError("offline ledger provenance invariant failed")
        identity = asdict(self)
        identity.pop("entry_id")
        encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode("utf-8")
        expected = hashlib.sha256(encoded).hexdigest()
        if self.entry_id != expected:
            raise EscalationError("ledger entry identity hash mismatch")


class EscalationLedger:
    def __init__(self, root: Path):
        self.root = root
        if root.is_symlink() or (root.exists() and not root.is_dir()):
            raise EscalationError("ledger root must be a real directory")
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.root.is_symlink() or not self.root.is_dir():
            raise EscalationError("ledger root must be a real directory")
        self.root.chmod(0o700)

    def append(self, entry: LedgerEntry) -> Path:
        entry.validate()
        path = self.root / f"{entry.entry_id}.json"
        data = json.dumps(asdict(entry), indent=2, sort_keys=True).encode("utf-8") + b"\n"
        try:
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError as error:
            raise EscalationError("ledger entry already exists") from error
        try:
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
        except Exception:
            path.unlink(missing_ok=True)
            raise
        return path

    def read_all(self) -> list[LedgerEntry]:
        entries: list[LedgerEntry] = []
        for path in sorted(self.root.glob("*.json")):
            if path.is_symlink() or not path.is_file():
                raise EscalationError("ledger entry must be a regular file")
            try:
                data = json.loads(path.read_text(encoding="utf-8"))
                if not isinstance(data, Mapping):
                    raise TypeError("not an object")
                data = dict(data)
                data["triggers"] = tuple(data.get("triggers", ()))
                entry = LedgerEntry(**data)
            except (OSError, json.JSONDecodeError, TypeError) as error:
                raise EscalationError(f"invalid ledger entry {path.name}: {error}") from error
            entry.validate()
            if path.name != f"{entry.entry_id}.json":
                raise EscalationError("ledger filename does not match entry identity")
            entries.append(entry)
        return entries
