"""A downloaded artifact that cannot be signature-verified is never installed.

`ados_verify_artifact` gates the prebuilt kernel modules. The `.sha256` beside
an artifact comes from the same host as the artifact, so it proves the transfer
and not the origin: whoever controls the download could withhold the
`.minisig` and ship a matching digest. A missing signature, a missing verifier
or a missing key is therefore refused, with no channel or flag that relaxes it.
"""

from __future__ import annotations

import hashlib
import shutil
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[1]
VERIFY_SH = REPO / "scripts" / "lib" / "verify.sh"

pytestmark = pytest.mark.skipif(
    shutil.which("bash") is None or shutil.which("sha256sum") is None,
    reason="bash and sha256sum are required to exercise the shell helper",
)


def _run(snippet: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", "-c", f'set -u; . "{VERIFY_SH}"\n{snippet}'],
        capture_output=True,
        text=True,
    )


def _artifact_with_sha(tmp_path: Path) -> Path:
    artifact = tmp_path / "thing.ko"
    artifact.write_bytes(b"payload")
    digest = hashlib.sha256(b"payload").hexdigest()
    (tmp_path / "thing.ko.sha256").write_text(f"{digest}  {artifact.name}\n")
    return artifact


def test_an_artifact_without_a_signature_is_refused(tmp_path: Path) -> None:
    artifact = _artifact_with_sha(tmp_path)
    res = _run(f'ados_verify_artifact "{artifact}" "SOMEPUBKEY"; echo "rc=$?"')
    assert "rc=0" not in res.stdout, "a sha-only artifact must not install"


def test_an_artifact_with_no_signing_key_is_refused(tmp_path: Path) -> None:
    artifact = _artifact_with_sha(tmp_path)
    res = _run(f'ados_verify_artifact "{artifact}" ""; echo "rc=$?"')
    assert "rc=0" not in res.stdout, "no trust anchor means nothing is proven"
