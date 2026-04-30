"""Static analysis for ``.adosplug`` archives.

Run before submitting a plugin to a registry to surface findings the
host would warn about. The analyzer reads an unsigned or signed
archive, examines manifest, agent payload, and GCS bundle, and returns
a report of findings keyed by severity.

The same logic runs at registry submission time on the server side
once the registry ships; for v0.1 it is local-only and advisory.
"""

from __future__ import annotations

import io
import re
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

from ados.plugins.archive import (
    ARCHIVE_MAX_BYTES,
    ENTRY_MAX_BYTES,
    MANIFEST_FILENAME,
    SIGNATURE_FILENAME,
    parse_archive_bytes,
)
from ados.plugins.errors import ArchiveError


SEVERITY_INFO = "info"
SEVERITY_WARN = "warn"
SEVERITY_ERROR = "error"
SEVERITY_CRITICAL = "critical"

SEVERITY_RANK = {
    SEVERITY_INFO: 0,
    SEVERITY_WARN: 1,
    SEVERITY_ERROR: 2,
    SEVERITY_CRITICAL: 3,
}


@dataclass(frozen=True)
class LintFinding:
    rule_id: str
    severity: str
    message: str
    file: str | None = None
    line: int | None = None


@dataclass
class LintReport:
    plugin_id: str
    version: str
    findings: list[LintFinding] = field(default_factory=list)
    archive_size: int = 0
    score: int = 100

    @property
    def passed(self) -> bool:
        return not any(
            f.severity in (SEVERITY_ERROR, SEVERITY_CRITICAL) for f in self.findings
        )

    def by_severity(self, severity: str) -> list[LintFinding]:
        return [f for f in self.findings if f.severity == severity]

    def to_dict(self) -> dict:
        return {
            "plugin_id": self.plugin_id,
            "version": self.version,
            "archive_size": self.archive_size,
            "score": self.score,
            "passed": self.passed,
            "findings": [
                {
                    "rule_id": f.rule_id,
                    "severity": f.severity,
                    "message": f.message,
                    "file": f.file,
                    "line": f.line,
                }
                for f in self.findings
            ],
        }


_PY_BANNED_PATTERNS: tuple[tuple[str, str, str], ...] = (
    (
        "PY001-os-system",
        r"\bos\.system\s*\(",
        "os.system call. Use the SDK's subprocess wrappers.",
    ),
    (
        "PY002-os-popen",
        r"\bos\.popen\s*\(",
        "os.popen call. Use the SDK's subprocess wrappers.",
    ),
    (
        "PY003-eval",
        r"(^|[^.])\beval\s*\(",
        "eval() call. Plugins must not evaluate dynamic code.",
    ),
    (
        "PY004-exec",
        r"(^|[^.])\bexec\s*\(",
        "exec() call. Plugins must not execute dynamic code.",
    ),
    (
        "PY005-subprocess-shell",
        r"subprocess\.(?:Popen|run|call|check_call|check_output)\([^)]*shell\s*=\s*True",
        "subprocess call with shell=True. Use shell=False with an argv list.",
    ),
    (
        "PY006-raw-socket",
        r"\bsocket\.socket\s*\(",
        "Raw socket. Use the SDK's network client so capability checks apply.",
    ),
    (
        "PY007-pickle-load",
        r"\bpickle\.(?:loads?|Unpickler)\s*\(",
        "pickle.load on untrusted input. Use json or msgpack.",
    ),
    (
        "PY008-marshal-loads",
        r"\bmarshal\.loads?\s*\(",
        "marshal.load is unsafe on untrusted input.",
    ),
    (
        "PY009-ctypes-load",
        r"\bctypes\.(?:CDLL|cdll\.LoadLibrary|WinDLL|windll)\s*\(",
        "ctypes loads a native library. Vendor binaries must be flagged in the manifest.",
    ),
)

_PY_NETWORK_PATTERNS: tuple[tuple[str, str, str], ...] = (
    (
        "PY020-requests",
        r"\b(?:requests|httpx|aiohttp|urllib3|urllib\.request)\b",
        "Network library imported. Plugin must declare network.outbound.",
    ),
)

_GCS_BANNED_PATTERNS: tuple[tuple[str, str, str], ...] = (
    (
        "GCS001-top-location",
        r"\btop\.location\b|\bparent\.location\b",
        "Tries to read parent or top location. Sandbox blocks this; signals intent.",
    ),
    (
        "GCS002-document-cookie",
        r"\bdocument\.cookie\b",
        "Reads document.cookie. Sandbox blocks this; signals intent.",
    ),
    (
        "GCS003-localstorage",
        r"\b(?:localStorage|sessionStorage)\b",
        "Accesses Web Storage. Use the SDK's host-mediated storage API.",
    ),
    (
        "GCS004-eval",
        r"(^|[^.\w])\beval\s*\(",
        "eval() in GCS bundle. Use static code paths.",
    ),
    (
        "GCS005-function-ctor",
        r"\bnew\s+Function\s*\(",
        "new Function() in GCS bundle. Use static code paths.",
    ),
    (
        "GCS006-fetch-direct",
        r"\bfetch\s*\(",
        "Direct fetch() in GCS bundle. Use the host bridge so capability checks apply.",
    ),
    (
        "GCS007-xhr-direct",
        r"\bnew\s+XMLHttpRequest\s*\(",
        "Direct XMLHttpRequest in GCS bundle. Use the host bridge.",
    ),
    (
        "GCS008-websocket-direct",
        r"\bnew\s+WebSocket\s*\(",
        "Direct WebSocket in GCS bundle. Use the host bridge.",
    ),
)

_FS_WRITE_PATTERNS: tuple[tuple[str, str, str], ...] = (
    (
        "FS001-open-write",
        r"open\s*\([^)]*['\"][rwab+]*w",
        "File opened for writing. Confirm path is under ctx.data_dir / config_dir / temp_dir.",
    ),
    (
        "FS002-shutil-rmtree",
        r"\bshutil\.rmtree\s*\(",
        "shutil.rmtree call. Confirm scope is the plugin's own directory.",
    ),
)


def _scan_text(
    text: str,
    file: str,
    rules: tuple[tuple[str, str, str], ...],
    severity: str,
) -> list[LintFinding]:
    findings: list[LintFinding] = []
    for rule_id, pattern, message in rules:
        for match in re.finditer(pattern, text):
            line = text.count("\n", 0, match.start()) + 1
            findings.append(
                LintFinding(
                    rule_id=rule_id,
                    severity=severity,
                    message=message,
                    file=file,
                    line=line,
                )
            )
    return findings


def lint_archive(path: str | Path) -> LintReport:
    """Run static analysis on a ``.adosplug`` archive.

    Returns a :class:`LintReport`. Does not raise on findings; raises
    :class:`ArchiveError` on malformed archives.
    """
    p = Path(path)
    raw = p.read_bytes()
    archive = parse_archive_bytes(raw)
    manifest = archive.manifest

    report = LintReport(
        plugin_id=manifest.id,
        version=manifest.version,
        archive_size=len(raw),
    )

    if len(raw) > ARCHIVE_MAX_BYTES:
        report.findings.append(
            LintFinding(
                rule_id="ARC001-archive-size",
                severity=SEVERITY_ERROR,
                message=f"archive {len(raw)} bytes exceeds cap {ARCHIVE_MAX_BYTES}",
            )
        )

    declared_perms = {p.id for p in (manifest.agent.permissions if manifest.agent else [])}
    declared_perms.update(
        {p.id for p in (manifest.gcs.permissions if manifest.gcs else [])}
    )
    has_network = "network.outbound" in declared_perms
    has_vendor_binary = bool(getattr(manifest, "contains_vendor_binary", False))

    zf = zipfile.ZipFile(io.BytesIO(raw))
    try:
        for info in zf.infolist():
            if info.filename.endswith("/"):
                continue
            if info.filename in (MANIFEST_FILENAME, SIGNATURE_FILENAME):
                continue
            if info.file_size > ENTRY_MAX_BYTES:
                report.findings.append(
                    LintFinding(
                        rule_id="ARC002-entry-size",
                        severity=SEVERITY_ERROR,
                        message=f"{info.filename}: {info.file_size} bytes exceeds per-entry cap",
                        file=info.filename,
                    )
                )
                continue

            try:
                data = zf.read(info.filename)
            except (zipfile.BadZipFile, OSError) as exc:
                report.findings.append(
                    LintFinding(
                        rule_id="ARC003-read-fail",
                        severity=SEVERITY_ERROR,
                        message=f"{info.filename}: read failed: {exc}",
                        file=info.filename,
                    )
                )
                continue

            name = info.filename
            lower = name.lower()

            if lower.endswith(".py"):
                try:
                    text = data.decode("utf-8", errors="replace")
                except UnicodeDecodeError:
                    continue
                report.findings.extend(_scan_text(text, name, _PY_BANNED_PATTERNS, SEVERITY_ERROR))
                report.findings.extend(_scan_text(text, name, _FS_WRITE_PATTERNS, SEVERITY_INFO))
                if not has_network:
                    report.findings.extend(
                        _scan_text(text, name, _PY_NETWORK_PATTERNS, SEVERITY_WARN)
                    )

            if lower.endswith((".js", ".mjs", ".ts", ".tsx")):
                try:
                    text = data.decode("utf-8", errors="replace")
                except UnicodeDecodeError:
                    continue
                report.findings.extend(_scan_text(text, name, _GCS_BANNED_PATTERNS, SEVERITY_WARN))
    finally:
        zf.close()

    if has_vendor_binary:
        report.findings.append(
            LintFinding(
                rule_id="VND001-vendor-binary",
                severity=SEVERITY_INFO,
                message="manifest declares contains_vendor_binary; archive runs in mandatory subprocess isolation",
            )
        )

    if archive.signer_id is None or archive.signature_b64 is None:
        report.findings.append(
            LintFinding(
                rule_id="SIG001-unsigned",
                severity=SEVERITY_WARN,
                message="archive is unsigned; registry submission requires a signed archive",
            )
        )

    high_risk_caps = {
        "vehicle.command",
        "vehicle.payload.actuate",
        "filesystem.host",
        "mavlink.command.send",
    }
    if declared_perms & high_risk_caps:
        report.findings.append(
            LintFinding(
                rule_id="PERM001-high-risk",
                severity=SEVERITY_INFO,
                message=(
                    "manifest declares high-risk capabilities: "
                    + ", ".join(sorted(declared_perms & high_risk_caps))
                ),
            )
        )

    severity_penalty = {
        SEVERITY_INFO: 0,
        SEVERITY_WARN: 2,
        SEVERITY_ERROR: 10,
        SEVERITY_CRITICAL: 25,
    }
    deduction = sum(severity_penalty[f.severity] for f in report.findings)
    report.score = max(0, 100 - deduction)
    return report


def format_report(report: LintReport) -> str:
    """Render a human-readable lint report."""
    lines: list[str] = []
    lines.append(f"plugin {report.plugin_id} {report.version}")
    lines.append(f"archive size: {report.archive_size} bytes")
    lines.append(f"score: {report.score}/100")
    lines.append(f"verdict: {'pass' if report.passed else 'fail'}")
    lines.append("")
    if not report.findings:
        lines.append("no findings.")
        return "\n".join(lines)
    for severity in (SEVERITY_CRITICAL, SEVERITY_ERROR, SEVERITY_WARN, SEVERITY_INFO):
        bucket = report.by_severity(severity)
        if not bucket:
            continue
        lines.append(f"[{severity}] {len(bucket)} finding(s)")
        for f in bucket:
            location = f.file or ""
            if f.line is not None:
                location = f"{location}:{f.line}"
            prefix = f"  {f.rule_id}"
            if location:
                prefix = f"{prefix} {location}"
            lines.append(f"{prefix}: {f.message}")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


__all__ = [
    "LintFinding",
    "LintReport",
    "SEVERITY_INFO",
    "SEVERITY_WARN",
    "SEVERITY_ERROR",
    "SEVERITY_CRITICAL",
    "format_report",
    "lint_archive",
]
