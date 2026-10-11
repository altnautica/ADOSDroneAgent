"""``ados plugin`` signing: sign and keygen."""

from __future__ import annotations

import os
import sys
from pathlib import Path

import click

from ados.cli._plugin_common import (
    _SIGNER_ID_RE,
    EXIT_GENERIC,
    EXIT_MANIFEST_INVALID,
    _emit_err,
    _emit_ok,
    plugin_group,
)
from ados.plugins.errors import ArchiveError, ManifestError


@plugin_group.command(
    "sign",
    help=(
        "Pack a plugin directory into a signed .adosplug archive. "
        "Signs the canonical payload hash with an Ed25519 private key "
        "(PEM-encoded) and writes the SIGNATURE file inside the archive."
    ),
)
@click.argument(
    "plugin_dir",
    type=click.Path(exists=True, file_okay=False, dir_okay=True),
)
@click.option(
    "--key",
    "key_path",
    required=True,
    type=click.Path(exists=True, dir_okay=False),
    help="Path to the Ed25519 private key in PEM format.",
)
@click.option(
    "--signer-id",
    "signer_id",
    required=True,
    help="Signer identifier written to the SIGNATURE file. "
    "Must match the public-key filename on the agent (signer-id.pem).",
)
@click.option(
    "--output",
    "output_path",
    required=True,
    type=click.Path(dir_okay=False),
    help="Output path for the signed .adosplug archive.",
)
@click.option("--json", "as_json", is_flag=True)
def sign_plugin(
    plugin_dir: str,
    key_path: str,
    signer_id: str,
    output_path: str,
    as_json: bool,
) -> None:
    """Sign a plugin directory and emit a ready-to-distribute archive.

    This is a developer-side command. It is intentionally a thin wrapper
    around the canonical archive packer and the well-known canonical
    payload hash, so the resulting archive matches what
    :func:`ados.plugins.archive.parse_archive_bytes` and the agent's
    signature verifier expect.
    """
    import base64
    import hashlib
    import io
    import zipfile

    from ados.plugins.archive import (
        MANIFEST_FILENAME,
        SIGNATURE_FILENAME,
        _canonical_payload_hash,
        pack_directory,
    )
    from ados.plugins.manifest import PluginManifest

    if not _SIGNER_ID_RE.fullmatch(signer_id):
        _emit_err(as_json, EXIT_GENERIC, f"invalid signer id {signer_id!r}")
        sys.exit(EXIT_GENERIC)

    plugin_root = Path(plugin_dir)
    manifest_path = plugin_root / "manifest.yaml"
    if not manifest_path.is_file():
        _emit_err(
            as_json,
            EXIT_MANIFEST_INVALID,
            f"manifest.yaml not found at {manifest_path}",
        )
        sys.exit(EXIT_MANIFEST_INVALID)

    try:
        manifest = PluginManifest.from_yaml_file(manifest_path)
    except ManifestError as exc:
        _emit_err(as_json, EXIT_MANIFEST_INVALID, str(exc))
        sys.exit(EXIT_MANIFEST_INVALID)

    output = Path(output_path)
    output.parent.mkdir(parents=True, exist_ok=True)

    # Pack the unsigned archive to a temp path first; the signing step
    # re-writes it with the SIGNATURE entry appended.
    import tempfile

    tmp_handle, tmp_name = tempfile.mkstemp(
        suffix=".adosplug", dir=str(output.parent)
    )
    os.close(tmp_handle)
    tmp_archive = Path(tmp_name)
    try:
        try:
            pack_directory(plugin_root, manifest, tmp_archive)
        except ArchiveError as exc:
            _emit_err(as_json, EXIT_GENERIC, str(exc))
            sys.exit(EXIT_GENERIC)

        # Load the just-packed entries to compute the canonical hash.
        # Use the in-memory bytes so the hash math is identical to what
        # the agent does at unpack time.
        raw = tmp_archive.read_bytes()
        entries: dict[str, bytes] = {}
        with zipfile.ZipFile(io.BytesIO(raw)) as zf:
            for info in zf.infolist():
                if info.filename.endswith("/"):
                    continue
                entries[info.filename] = zf.read(info.filename)

        if MANIFEST_FILENAME not in entries:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"packed archive missing {MANIFEST_FILENAME}",
            )
            sys.exit(EXIT_GENERIC)

        payload_hash = _canonical_payload_hash(entries)

        # Load the Ed25519 private key: an unencrypted PKCS#8 PEM, which is
        # what `ados plugin keygen` writes. Anything else is refused with the
        # reason; a public key or an arbitrary file must never be read as a
        # private seed and produce a signature under a key nobody enrolled.
        try:
            from cryptography.hazmat.primitives.asymmetric.ed25519 import (
                Ed25519PrivateKey,
            )
            from cryptography.hazmat.primitives.serialization import (
                load_pem_private_key,
            )
        except ImportError as exc:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"cryptography package required: {exc}",
                hint="pip install cryptography",
            )
            sys.exit(EXIT_GENERIC)

        key_bytes = Path(key_path).read_bytes()
        try:
            private = load_pem_private_key(key_bytes, password=None)
        except TypeError:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"{key_path} is an encrypted private key; signing needs an "
                "unencrypted PKCS#8 PEM",
            )
            sys.exit(EXIT_GENERIC)
        except ValueError as exc:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"{key_path} is not a PEM private key: {exc}",
                hint="Pass the <signer-id>.priv.pem written by `ados plugin keygen`.",
            )
            sys.exit(EXIT_GENERIC)

        if not isinstance(private, Ed25519PrivateKey):
            _emit_err(
                as_json,
                EXIT_GENERIC,
                "private key is not an Ed25519 key",
            )
            sys.exit(EXIT_GENERIC)

        signature = private.sign(payload_hash)
        sig_b64 = base64.b64encode(signature).decode("ascii")

        # Write the final archive: original entries plus SIGNATURE.
        # Re-pack from scratch (rather than mutating the existing zip
        # in place) so the byte stream is deterministic.
        if output.exists():
            output.unlink()
        with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.writestr(
                MANIFEST_FILENAME, entries[MANIFEST_FILENAME]
            )
            for name in sorted(entries):
                if name in (MANIFEST_FILENAME, SIGNATURE_FILENAME):
                    continue
                zf.writestr(name, entries[name])
            zf.writestr(
                SIGNATURE_FILENAME, f"{signer_id}\n{sig_b64}\n"
            )

        sha256 = hashlib.sha256(output.read_bytes()).hexdigest()
        sums_path = output.with_suffix(output.suffix + ".sha256")
        sums_path.write_text(
            f"{sha256}  {output.name}\n", encoding="utf-8"
        )

        result = {
            "plugin_id": manifest.id,
            "version": manifest.version,
            "signer_id": signer_id,
            "signature_b64": sig_b64,
            "payload_hash_hex": payload_hash.hex(),
            "archive": str(output),
            "sha256": sha256,
            "sha256_file": str(sums_path),
        }
        if as_json:
            _emit_ok(as_json, result)
        else:
            click.echo(f"Plugin:    {manifest.id} v{manifest.version}")
            click.echo(f"Signer:    {signer_id}")
            click.echo(f"Signature: {sig_b64}")
            click.echo(f"SHA-256:   {sha256}")
            click.echo(f"Archive:   {output}")
            click.echo(f"Checksum:  {sums_path}")
    finally:
        if tmp_archive.exists():
            tmp_archive.unlink()


@plugin_group.command(
    "keygen",
    help=(
        "Generate a fresh Ed25519 keypair for plugin signing. "
        "Developer aid: do NOT use this to mint production publisher "
        "keys without an offline workflow for the private half."
    ),
)
@click.argument("signer_id")
@click.option(
    "--output-dir",
    "output_dir",
    required=True,
    type=click.Path(file_okay=False),
    help="Directory where <signer-id>.pem and <signer-id>.priv.pem will be written.",
)
@click.option(
    "--force",
    is_flag=True,
    help="Overwrite existing key files at the target paths.",
)
@click.option("--json", "as_json", is_flag=True)
def keygen(
    signer_id: str, output_dir: str, force: bool, as_json: bool
) -> None:
    """Mint a fresh Ed25519 keypair.

    Writes two files under ``output_dir``:

    * ``<signer-id>.pem`` — public key in PEM (mode 0644)
    * ``<signer-id>.priv.pem`` — private key in PEM (mode 0600)

    The public PEM is what installs onto the agent at
    ``/etc/ados/plugin-keys/<signer-id>.pem``. The private half stays
    with the signing rig — never check it in, never copy it across hosts
    without an encrypted transport. The developer-aid framing matters:
    production publisher keys deserve a hardware-token or air-gapped
    workflow, not a one-liner.
    """
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import (
            Ed25519PrivateKey,
        )
        from cryptography.hazmat.primitives.serialization import (
            Encoding,
            NoEncryption,
            PrivateFormat,
            PublicFormat,
        )
    except ImportError as exc:
        _emit_err(
            as_json,
            EXIT_GENERIC,
            f"cryptography package required: {exc}",
            hint="pip install cryptography",
        )
        sys.exit(EXIT_GENERIC)

    if not _SIGNER_ID_RE.fullmatch(signer_id):
        _emit_err(
            as_json,
            EXIT_GENERIC,
            f"invalid signer id {signer_id!r}",
            hint="Use letters, digits, '.', '_' and '-' (up to 64), starting "
            "with a letter or digit.",
        )
        sys.exit(EXIT_GENERIC)

    out_dir = Path(output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    pub_path = out_dir / f"{signer_id}.pem"
    priv_path = out_dir / f"{signer_id}.priv.pem"

    for path in (pub_path, priv_path):
        if path.exists() and not force:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"{path} already exists",
                hint="Pass --force to overwrite, or pick a different signer-id.",
            )
            sys.exit(EXIT_GENERIC)

    private = Ed25519PrivateKey.generate()
    public = private.public_key()

    priv_pem = private.private_bytes(
        encoding=Encoding.PEM,
        format=PrivateFormat.PKCS8,
        encryption_algorithm=NoEncryption(),
    )
    pub_pem = public.public_bytes(
        encoding=Encoding.PEM,
        format=PublicFormat.SubjectPublicKeyInfo,
    )

    pub_path.write_bytes(pub_pem)
    pub_path.chmod(0o644)
    # Created 0600 from the first byte: writing first and chmod-ing after
    # leaves the private key readable under the umask in between. An
    # existing file (--force) is replaced, not rewritten in place, so its
    # old mode never applies either.
    priv_path.unlink(missing_ok=True)
    fd = os.open(str(priv_path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as fh:
        fh.write(priv_pem)

    # Fingerprint = SHA-256 of the raw public-key bytes, base64 encoded
    # short-form. Stable, copy-pasteable identifier for the operator to
    # cross-check against what's installed on the agent.
    import base64
    import hashlib

    raw_pub = public.public_bytes(
        encoding=Encoding.Raw, format=PublicFormat.Raw
    )
    fp = base64.b64encode(hashlib.sha256(raw_pub).digest()).decode(
        "ascii"
    )[:22]

    result = {
        "signer_id": signer_id,
        "public_key_path": str(pub_path),
        "private_key_path": str(priv_path),
        "fingerprint": fp,
    }
    if as_json:
        _emit_ok(as_json, result)
    else:
        click.echo(f"Signer ID:   {signer_id}")
        click.echo(f"Public key:  {pub_path}")
        click.echo(f"Private key: {priv_path} (mode 0600)")
        click.echo(f"Fingerprint: {fp}")
        click.echo("")
        click.echo(
            "Install the public key on the agent at "
            f"/etc/ados/plugin-keys/{signer_id}.pem"
        )
        click.echo(
            "and keep the private key offline. Never commit *.priv.pem to git."
        )
