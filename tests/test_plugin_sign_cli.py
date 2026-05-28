"""Coverage for ``ados plugin sign`` and ``ados plugin keygen``.

Generates a throwaway keypair, packs a fixture plugin into a signed
archive, then walks the resulting archive back through the agent's
canonical signature-verification path to confirm the bytes match what
the verifier expects.
"""

from __future__ import annotations

import base64
import json
import zipfile
from pathlib import Path

from click.testing import CliRunner

from ados.cli.plugin import plugin_group
from ados.plugins.archive import (
    MANIFEST_FILENAME,
    SIGNATURE_FILENAME,
    open_archive,
)
from ados.plugins.signing import (
    TrustedKey,
    verify_archive_signature,
)

_MANIFEST_YAML = """\
schema_version: 1
id: com.example.signed
version: 0.1.0
name: Signed Example
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions:
    - event.publish
"""


def _write_plugin(root: Path) -> None:
    (root / "manifest.yaml").write_text(_MANIFEST_YAML, encoding="utf-8")
    agent_dir = root / "agent"
    agent_dir.mkdir()
    (agent_dir / "plugin.py").write_text(
        "def main():\n    return 'hello'\n", encoding="utf-8"
    )


def test_keygen_writes_keypair(tmp_path: Path) -> None:
    runner = CliRunner()
    result = runner.invoke(
        plugin_group,
        [
            "keygen",
            "example-signer-A",
            "--output-dir",
            str(tmp_path),
            "--json",
        ],
    )
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output.strip().splitlines()[-1])
    assert payload["ok"] is True
    pub_path = Path(payload["data"]["public_key_path"])
    priv_path = Path(payload["data"]["private_key_path"])

    assert pub_path.exists()
    assert priv_path.exists()
    assert (priv_path.stat().st_mode & 0o777) == 0o600
    assert pub_path.read_bytes().startswith(b"-----BEGIN PUBLIC KEY-----")
    assert priv_path.read_bytes().startswith(
        b"-----BEGIN PRIVATE KEY-----"
    )


def test_keygen_refuses_overwrite_without_force(tmp_path: Path) -> None:
    runner = CliRunner()
    first = runner.invoke(
        plugin_group,
        ["keygen", "dup", "--output-dir", str(tmp_path), "--json"],
    )
    assert first.exit_code == 0

    second = runner.invoke(
        plugin_group,
        ["keygen", "dup", "--output-dir", str(tmp_path), "--json"],
    )
    assert second.exit_code != 0
    body = json.loads(second.output.strip().splitlines()[-1])
    assert body["ok"] is False


def test_keygen_force_overwrites(tmp_path: Path) -> None:
    runner = CliRunner()
    first = runner.invoke(
        plugin_group,
        ["keygen", "dup2", "--output-dir", str(tmp_path), "--json"],
    )
    assert first.exit_code == 0
    pub = tmp_path / "dup2.pem"
    original = pub.read_bytes()

    second = runner.invoke(
        plugin_group,
        [
            "keygen",
            "dup2",
            "--output-dir",
            str(tmp_path),
            "--force",
            "--json",
        ],
    )
    assert second.exit_code == 0
    assert pub.read_bytes() != original


def test_sign_round_trip_verifies(tmp_path: Path) -> None:
    """Sign a plugin and verify it via the agent's normal signature path."""
    runner = CliRunner()
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()

    signer_id = "example-signer-B"

    keygen_result = runner.invoke(
        plugin_group,
        [
            "keygen",
            signer_id,
            "--output-dir",
            str(keys_dir),
            "--json",
        ],
    )
    assert keygen_result.exit_code == 0

    plugin_dir = tmp_path / "plugin"
    plugin_dir.mkdir()
    _write_plugin(plugin_dir)

    output = tmp_path / "out" / "signed.adosplug"

    sign_result = runner.invoke(
        plugin_group,
        [
            "sign",
            str(plugin_dir),
            "--key",
            str(keys_dir / f"{signer_id}.priv.pem"),
            "--signer-id",
            signer_id,
            "--output",
            str(output),
            "--json",
        ],
    )
    assert sign_result.exit_code == 0, sign_result.output

    payload = json.loads(sign_result.output.strip().splitlines()[-1])
    assert payload["ok"] is True
    data = payload["data"]
    assert data["signer_id"] == signer_id
    assert data["plugin_id"] == "com.example.signed"
    assert data["version"] == "0.1.0"

    # Sidecar SHA-256 is written and matches the archive bytes.
    sums = Path(data["sha256_file"])
    assert sums.exists()
    sums_line = sums.read_text(encoding="utf-8").strip()
    sha256_hex, fname = sums_line.split()
    assert fname == output.name
    assert sha256_hex == data["sha256"]

    # Parse the archive the same way the agent does, then run the
    # signature verifier against the public key minted by keygen.
    contents = open_archive(output)
    assert contents.signer_id == signer_id
    assert contents.signature_b64 == data["signature_b64"]

    pub_pem = (keys_dir / f"{signer_id}.pem").read_bytes()
    trusted = {
        signer_id: TrustedKey(signer_id=signer_id, pem=pub_pem)
    }
    # No exception ⇒ signature verifies.
    verify_archive_signature(
        contents.payload_hash,
        contents.signature_b64,
        contents.signer_id,
        trusted_keys=trusted,
        revocations=set(),
    )


def test_sign_emits_correct_signature_file_format(tmp_path: Path) -> None:
    """SIGNATURE file inside the archive matches the 2-line contract.

    Line 1 = signer id, line 2 = base64 signature, trailing newline.
    The agent's archive parser rejects anything else.
    """
    runner = CliRunner()
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    signer_id = "fmt-check"

    assert (
        runner.invoke(
            plugin_group,
            [
                "keygen",
                signer_id,
                "--output-dir",
                str(keys_dir),
                "--json",
            ],
        ).exit_code
        == 0
    )

    plugin_dir = tmp_path / "plugin"
    plugin_dir.mkdir()
    _write_plugin(plugin_dir)

    output = tmp_path / "signed.adosplug"
    assert (
        runner.invoke(
            plugin_group,
            [
                "sign",
                str(plugin_dir),
                "--key",
                str(keys_dir / f"{signer_id}.priv.pem"),
                "--signer-id",
                signer_id,
                "--output",
                str(output),
                "--json",
            ],
        ).exit_code
        == 0
    )

    with zipfile.ZipFile(output) as zf:
        names = set(zf.namelist())
        assert SIGNATURE_FILENAME in names
        assert MANIFEST_FILENAME in names
        sig_text = zf.read(SIGNATURE_FILENAME).decode("utf-8")

    lines = [line for line in sig_text.splitlines() if line.strip()]
    assert len(lines) == 2
    assert lines[0] == signer_id
    # Line 2 is valid base64 and decodes to a 64-byte Ed25519 signature.
    decoded = base64.b64decode(lines[1])
    assert len(decoded) == 64


def test_sign_rejects_missing_manifest(tmp_path: Path) -> None:
    runner = CliRunner()
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    runner.invoke(
        plugin_group,
        [
            "keygen",
            "stub",
            "--output-dir",
            str(keys_dir),
            "--json",
        ],
    )
    bad_dir = tmp_path / "empty"
    bad_dir.mkdir()

    result = runner.invoke(
        plugin_group,
        [
            "sign",
            str(bad_dir),
            "--key",
            str(keys_dir / "stub.priv.pem"),
            "--signer-id",
            "stub",
            "--output",
            str(tmp_path / "out.adosplug"),
            "--json",
        ],
    )
    assert result.exit_code == 2  # EXIT_MANIFEST_INVALID
    payload = json.loads(result.output.strip().splitlines()[-1])
    assert payload["kind"] == "manifest_invalid"
