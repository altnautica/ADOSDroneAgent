"""An unrecognised release channel must not skip signature verification.

`ados_verify_artifact` decides what to do with an artifact it CANNOT verify —
no signature published, or no verifier installed. The rule was written as
"strict only when the channel is exactly stable", which reads correctly for the
two known channels and silently opens a hole for any third value.

The kernel-module path passed its own channel name. It matched neither, so it
took the lenient branch and warned-and-passed on every unverifiable module —
the one artifact class where tampering matters most, and the one place nobody
looked because the code appeared to have a channel gate.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[1]
VERIFY_SH = REPO / "scripts" / "lib" / "verify.sh"

pytestmark = pytest.mark.skipif(
    shutil.which("bash") is None, reason="bash is required to exercise the shell helper"
)


def _run(snippet: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", "-c", f'set -u; . "{VERIFY_SH}"\n{snippet}'],
        capture_output=True,
        text=True,
    )


@pytest.mark.parametrize("channel", ["edge"])
def test_the_development_channel_is_lenient(channel: str) -> None:
    assert _run(f'ados_channel_is_lenient "{channel}"').returncode == 0


@pytest.mark.parametrize(
    "channel",
    [
        "stable",
        # The value the kernel-module path actually passed. This is the bug.
        "prebuilt",
        # A typo must fail closed, not open.
        "edgee",
        "Edge",
        "",
    ],
)
def test_every_other_channel_is_strict(channel: str) -> None:
    assert _run(f'ados_channel_is_lenient "{channel}"').returncode != 0, (
        f"channel {channel!r} must not be treated as lenient — an unrecognised "
        "channel is not a licence to skip a signature"
    )


def test_an_unverifiable_artifact_is_refused_on_an_unknown_channel(tmp_path: Path) -> None:
    artifact = tmp_path / "thing.ko"
    artifact.write_bytes(b"payload")
    # A matching sha256 so only the signature decision is under test.
    digest = subprocess.run(
        ["shasum", "-a", "256", artifact.name],
        cwd=tmp_path,
        capture_output=True,
        text=True,
    ).stdout.split()[0]
    (tmp_path / "thing.ko.sha256").write_text(f"{digest}  {artifact.name}\n")

    # No .minisig on disk => unverifiable. With a pubkey set and the channel the
    # driver path used to pass, this must now fail.
    res = _run(
        f'ados_verify_artifact "{artifact}" "SOMEPUBKEY" "prebuilt" 0; echo "rc=$?"'
    )
    assert "rc=0" not in res.stdout, (
        "an unverifiable kernel module must not install on an unknown channel"
    )
