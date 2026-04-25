"""X.509 certificate generation and TLS context creation using ECDSA P-256."""

from __future__ import annotations

import datetime
import ssl
from pathlib import Path

import structlog
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

from ados.core.paths import CERTS_DIR

log = structlog.get_logger(__name__)

DEFAULT_CERT_DIR = str(CERTS_DIR)


def generate_self_signed_cert(
    common_name: str,
    days_valid: int = 365,
    cert_dir: str | None = None,
) -> tuple[bytes, bytes]:
    """Generate a self-signed X.509 certificate with ECDSA P-256.

    Args:
        common_name: The CN field for the certificate subject/issuer.
        days_valid: How many days the certificate remains valid.
        cert_dir: Optional directory to save cert.pem and key.pem.
                  If None, files are not written to disk.

    Returns:
        A tuple of (cert_pem, key_pem) as PEM-encoded bytes.
    """
    log.info("generating_self_signed_cert", cn=common_name, days=days_valid)

    key = ec.generate_private_key(ec.SECP256R1())

    subject = issuer = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, common_name),
    ])

    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now)
        .not_valid_after(now + datetime.timedelta(days=days_valid))
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName(common_name)]),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )

    cert_pem = cert.public_bytes(serialization.Encoding.PEM)
    key_pem = key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.TraditionalOpenSSL,
        serialization.NoEncryption(),
    )

    if cert_dir is not None:
        _save_to_disk(cert_pem, key_pem, cert_dir)

    log.info(
        "cert_generated",
        cn=common_name,
        serial=cert.serial_number,
        not_after=cert.not_valid_after_utc.isoformat(),
    )
    return cert_pem, key_pem


def generate_csr(common_name: str, key_pem: bytes) -> bytes:
    """Generate a Certificate Signing Request from an existing private key.

    Args:
        common_name: The CN field for the CSR subject.
        key_pem: PEM-encoded private key bytes.

    Returns:
        PEM-encoded CSR bytes.
    """
    log.info("generating_csr", cn=common_name)

    key = serialization.load_pem_private_key(key_pem, password=None)

    csr = (
        x509.CertificateSigningRequestBuilder()
        .subject_name(x509.Name([
            x509.NameAttribute(NameOID.COMMON_NAME, common_name),
        ]))
        .sign(key, hashes.SHA256())  # type: ignore[arg-type]
    )

    csr_pem = csr.public_bytes(serialization.Encoding.PEM)
    log.info("csr_generated", cn=common_name)
    return csr_pem


def load_cert(cert_pem: bytes) -> x509.Certificate:
    """Load an X.509 certificate from PEM bytes.

    Args:
        cert_pem: PEM-encoded certificate bytes.

    Returns:
        Parsed x509.Certificate object.
    """
    cert = x509.load_pem_x509_certificate(cert_pem)
    log.debug("cert_loaded", cn=cert.subject.rfc4514_string())
    return cert


def create_tls_context(
    cert_path: str,
    key_path: str,
    ca_path: str | None = None,
) -> ssl.SSLContext:
    """Create a TLS 1.3 SSL context for server or mutual-TLS client use.

    Args:
        cert_path: Path to the PEM certificate file.
        key_path: Path to the PEM private key file.
        ca_path: Optional path to CA certificate for client verification.

    Returns:
        Configured ssl.SSLContext with TLS 1.3 minimum.
    """
    log.info("creating_tls_context", cert=cert_path, ca=ca_path)

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3

    ctx.load_cert_chain(certfile=cert_path, keyfile=key_path)

    if ca_path is not None:
        ctx.load_verify_locations(cafile=ca_path)
        ctx.verify_mode = ssl.CERT_REQUIRED
    else:
        # Self-signed mode: don't verify peer certs
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE

    log.info("tls_context_created", tls_min="1.3", verify=ctx.verify_mode.name)
    return ctx


def _save_to_disk(cert_pem: bytes, key_pem: bytes, cert_dir: str) -> None:
    """Write certificate and key files to disk with appropriate permissions."""
    dirpath = Path(cert_dir)
    try:
        dirpath.mkdir(parents=True, exist_ok=True)
    except PermissionError:
        log.warning("cert_dir_permission_denied", path=cert_dir)
        return

    cert_file = dirpath / "cert.pem"
    key_file = dirpath / "key.pem"

    cert_file.write_bytes(cert_pem)
    key_file.write_bytes(key_pem)

    # Restrict key file permissions (owner read only)
    try:
        key_file.chmod(0o600)
    except OSError as exc:
        log.warning("chmod_failed", path=str(key_file), error=str(exc))

    log.info("certs_saved", cert=str(cert_file), key=str(key_file))
