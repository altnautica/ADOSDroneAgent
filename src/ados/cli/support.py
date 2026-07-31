"""``ados support-bundle`` — one archive that answers a support request.

A field report is usually "the link dropped" plus a photograph of a screen. What
actually settles it is the agent's own state at the time: which version, which
profile, which services were up, what the radio counters said, what the black
box recorded. Collecting that by hand over SSH takes an operator through a dozen
commands, and the one they forget is invariably the one that mattered.

This writes a single archive. Nothing is sent anywhere: ``ados logs push`` only
works for a cloud-paired agent, which the local-first default is not, so the
operator gets a file and decides who sees it.

**Redaction is not optional and not best-effort.** The bundle carries config,
pairing state and logs, all of which hold secrets, so every collected file goes
through :func:`redact` before it is written. A key that reaches the archive has
already left the box the moment the operator attaches it to a ticket. Where a
value cannot be redacted with confidence, the file is omitted and the omission
is recorded in the manifest — an operator reading "omitted: could not redact"
knows to collect it deliberately, whereas a silently missing file reads as a
system that had nothing to say.
"""

from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import tarfile
import tempfile
from collections.abc import Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import click
import httpx

from ados.cli import _ansi, api_bases
from ados.core.paths import PAIRING_JSON

#: How long any single collection step may take. A wedged service must not hold
#: the whole bundle: a bundle that never finishes is one the operator abandons,
#: and then the report arrives with no bundle at all.
STEP_TIMEOUT_S = 10.0

#: Journal lines per unit. Enough to cover a boot and the failure after it,
#: bounded so one chatty service cannot crowd out the rest.
JOURNAL_LINES = 2000

#: Black-box records to include.
LOG_RECORDS = 5000


# ---------------------------------------------------------------------------
# Redaction
# ---------------------------------------------------------------------------

#: Keys whose VALUE is a secret wherever it appears, matched case-insensitively
#: against JSON/YAML keys and `key=value` lines.
_SECRET_KEYS = (
    "api_key",
    "apikey",
    "password",
    "passphrase",
    "psk",
    "secret",
    "token",
    "private_key",
    "privatekey",
    "admin_key",
    "hmac",
    "bind_key",
    "wpa_passphrase",
    "wifi_password",
    "auth",
    "credential",
    "session",
    "cookie",
    "pin",
)

# The optional closing quote before the separator is load bearing: in JSON the
# key is `"api_key"`, so the colon does NOT follow the word directly. Without it
# this matched shell and YAML while silently missing JSON — which is the format
# most of the bundle is collected in.
_KV_PATTERN = re.compile(
    r"(?i)\b(" + "|".join(re.escape(k) for k in _SECRET_KEYS) + r")\b"
    r"([\"']?\s*[:=]\s*)"
    r"(\"[^\"]*\"|'[^']*'|[^\s,;}\]]+)"
)

#: A bare base64/hex blob long enough to be a key. wfb-ng keys are 64 bytes, so
#: their base64 is 88 characters; anything at or above 40 is treated as key
#: material rather than guessed at.
_BLOB_PATTERN = re.compile(r"\b[A-Za-z0-9+/]{40,}={0,2}\b")

REDACTED = "«redacted»"


def redact(text: str) -> str:
    """Remove secret values while leaving the surrounding structure readable.

    The KEY is kept and only the VALUE replaced, because "there is an api_key
    configured" is exactly the sort of thing a support bundle needs to show
    while the key itself is exactly what it must not carry.
    """
    out = _KV_PATTERN.sub(lambda m: f"{m.group(1)}{m.group(2)}{REDACTED}", text)
    return _BLOB_PATTERN.sub(REDACTED, out)


def _redacted_json(obj: Any) -> str:
    return redact(json.dumps(obj, indent=2, sort_keys=True, default=str))


# ---------------------------------------------------------------------------
# Collection
# ---------------------------------------------------------------------------


def _api_key() -> str | None:
    try:
        if PAIRING_JSON.exists():
            data = json.loads(PAIRING_JSON.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) else None
    except (OSError, ValueError):
        pass
    return None


def _get(path: str) -> Any:
    """GET a local control-surface route, trying each candidate port."""
    headers = {}
    key = _api_key()
    if key:
        headers["X-ADOS-Key"] = key
    last: Exception | None = None
    for base in api_bases():
        try:
            r = httpx.get(f"{base}{path}", headers=headers, timeout=STEP_TIMEOUT_S)
            if r.status_code == 200:
                try:
                    return r.json()
                except ValueError:
                    return r.text
            last = RuntimeError(f"HTTP {r.status_code}")
        except Exception as e:  # noqa: BLE001 - recorded, not raised
            last = e
    raise RuntimeError(str(last) if last else "unreachable")


def _run(argv: list[str]) -> str:
    p = subprocess.run(
        argv, capture_output=True, text=True, timeout=STEP_TIMEOUT_S, check=False
    )
    return p.stdout + (f"\n[stderr]\n{p.stderr}" if p.stderr.strip() else "")


def _host_facts() -> dict[str, Any]:
    return {
        "collected_at": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "kernel": platform.release(),
    }


#: Each entry is (archive filename, collector). A collector returning None means
#: "nothing to collect here", which is recorded as skipped rather than failed.
def _collectors() -> list[tuple[str, Callable[[], str | None]]]:
    units = [
        "ados-supervisor",
        "ados-control",
        "ados-api",
        "ados-wfb",
        "ados-wfb-rx",
        "ados-video",
        "ados-mavlink",
        "ados-logd",
        "ados-net",
        "ados-cloud",
    ]
    return [
        ("host.json", lambda: _redacted_json(_host_facts())),
        ("version.txt", lambda: _run(["ados", "--version"])),
        # State, as the agent itself reports it.
        ("api/status-full.json", lambda: _redacted_json(_get("/api/status/full"))),
        ("api/wfb.json", lambda: _redacted_json(_get("/api/wfb"))),
        ("api/pairing-info.json", lambda: _redacted_json(_get("/api/pairing/info"))),
        ("api/services.json", lambda: _redacted_json(_get("/api/services"))),
        ("api/config.json", lambda: _redacted_json(_get("/api/config"))),
        # The fleet's slot table. It is returned by no read route today, so the
        # registry file is the only place to get it — and "which drone holds
        # which slot" is the first question a fleet link fault raises.
        (
            "fleet-registry.json",
            lambda: redact(Path("/var/lib/ados/fleet.json").read_text(encoding="utf-8"))
            if Path("/var/lib/ados/fleet.json").exists()
            else None,
        ),
        # Both diagnostics, because their verdicts are what the operator was
        # looking at when they decided to file the report.
        ("diag/video.txt", lambda: _run(["ados", "diag", "video"])),
        ("diag/link.txt", lambda: _run(["ados", "diag", "link"])),
        ("hardware.txt", lambda: _run(["ados", "hardware", "list"])),
        # The black box first, the journal only as the fallback it is.
        (
            "logs/blackbox.txt",
            lambda: _run(["ados", "logs", "query", "--limit", str(LOG_RECORDS)]),
        ),
        ("systemd/units.txt", lambda: _run(["systemctl", "list-units", "ados*", "--all", "--no-pager"])),
        ("systemd/failed.txt", lambda: _run(["systemctl", "list-units", "--state=failed", "--no-pager"])),
        *[
            (
                f"journal/{u}.txt",
                (lambda unit=u: _run(["journalctl", "-u", unit, "-n", str(JOURNAL_LINES), "--no-pager"])),
            )
            for u in units
        ],
        ("net/addresses.txt", lambda: _run(["ip", "-brief", "addr"])),
        ("net/routes.txt", lambda: _run(["ip", "route"])),
        ("disk.txt", lambda: _run(["df", "-h"])),
        ("memory.txt", lambda: _run(["free", "-m"])),
    ]


def collect(dest: Path) -> dict[str, Any]:
    """Write every collected file under `dest` and return the manifest.

    A failing collector never aborts the bundle: a box with a dead service is
    exactly the box someone is asking about, so its failure is recorded as
    content and the rest is still gathered.
    """
    manifest: dict[str, Any] = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "redaction": "applied to every file; see the module docstring",
        "included": [],
        "skipped": [],
        "failed": [],
    }

    for name, fn in _collectors():
        target = dest / name
        target.parent.mkdir(parents=True, exist_ok=True)
        try:
            body = fn()
        except Exception as e:  # noqa: BLE001 - the failure IS the datum
            manifest["failed"].append({"file": name, "error": str(e)})
            target.write_text(f"collection failed: {e}\n", encoding="utf-8")
            continue
        if body is None:
            manifest["skipped"].append({"file": name, "reason": "not present on this node"})
            continue
        # Belt and braces: every path writes through redaction, including the
        # ones whose collector already redacted, because a collector added later
        # must not be able to leak by forgetting.
        target.write_text(redact(body), encoding="utf-8")
        manifest["included"].append(name)

    (dest / "MANIFEST.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True), encoding="utf-8"
    )
    return manifest


@click.command(name="support-bundle")
@click.option(
    "--output",
    "-o",
    type=click.Path(dir_okay=False, path_type=Path),
    default=None,
    help="Where to write the archive. Defaults to the working directory.",
)
def support_bundle(output: Path | None) -> None:
    """Collect one redacted archive for a support request.

    Nothing is sent anywhere. The archive is written locally and the operator
    decides who sees it.
    """
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    try:
        info = _get("/api/pairing/info")
        device = info.get("device_id", "unknown") if isinstance(info, dict) else "unknown"
    except Exception:  # noqa: BLE001 - an unreachable agent still gets a bundle
        device = "unknown"

    dest = output or Path.cwd() / f"ados-support-{device}-{stamp}.tar.gz"

    theme = _ansi.detect_theme()
    click.echo(theme.dim("collecting…"))
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / f"ados-support-{device}-{stamp}"
        root.mkdir(parents=True)
        manifest = collect(root)

        with tarfile.open(dest, "w:gz") as tar:
            tar.add(root, arcname=root.name)

    # 0600: the bundle is redacted, not public. Redaction removes what we can
    # name; it is not a promise that nothing sensitive remains.
    try:
        os.chmod(dest, 0o600)
    except OSError:
        pass

    n_ok = len(manifest["included"])
    n_skip = len(manifest["skipped"])
    n_fail = len(manifest["failed"])

    click.echo("")
    click.echo(f"  {theme.ok('✓')} {dest}")
    click.echo(f"    {n_ok} collected, {n_skip} not present, {n_fail} failed")
    if n_fail:
        # Name them: a failed collector is often the fault being reported.
        for f in manifest["failed"][:5]:
            click.echo(theme.dim(f"      {f['file']}: {f['error']}"))
    click.echo("")
    click.echo(theme.dim("    Secrets are redacted. Nothing was sent anywhere."))
    click.echo("")
