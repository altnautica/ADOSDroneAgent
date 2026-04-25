"""WFB-ng encryption key management."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import WFB_KEY_DIR

log = get_logger("wfb.key_mgr")

DEFAULT_KEY_DIR = str(WFB_KEY_DIR)
TX_KEY_NAME = "tx.key"
RX_KEY_NAME = "rx.key"

# WFB-ng key file size: 32 bytes (NaCl crypto_box keypair seed)
WFB_KEY_SIZE = 32


def key_exists(key_dir: str | None = None) -> bool:
    """Check if both tx.key and rx.key exist in the key directory."""
    base = Path(key_dir or DEFAULT_KEY_DIR)
    tx_path = base / TX_KEY_NAME
    rx_path = base / RX_KEY_NAME
    return tx_path.is_file() and rx_path.is_file()


def load_key(path: str) -> bytes:
    """Load a WFB-ng key file and return its raw bytes.

    Args:
        path: Absolute or relative path to the key file.

    Returns:
        Raw key bytes.

    Raises:
        FileNotFoundError: If the key file does not exist.
        ValueError: If the key file is empty.
    """
    key_path = Path(path)
    if not key_path.is_file():
        raise FileNotFoundError(f"Key file not found: {path}")

    data = key_path.read_bytes()
    if not data:
        raise ValueError(f"Key file is empty: {path}")

    log.info("key_loaded", path=path, size=len(data))
    return data


def _generate_with_wfb_keygen(output_dir: Path) -> tuple[str, str]:
    """Generate keys using the wfb_keygen binary (preferred method).

    WFB-ng ships with `wfb_keygen` that produces a compatible keypair.
    The binary writes gs.key and drone.key to the current directory.
    We rename them to tx.key and rx.key.
    """
    result = subprocess.run(
        ["wfb_keygen"],
        capture_output=True,
        text=True,
        timeout=10,
        cwd=str(output_dir),
    )

    if result.returncode != 0:
        raise RuntimeError(f"wfb_keygen failed: {result.stderr.strip()}")

    # wfb_keygen creates gs.key and drone.key
    gs_key = output_dir / "gs.key"
    drone_key = output_dir / "drone.key"

    tx_path = output_dir / TX_KEY_NAME
    rx_path = output_dir / RX_KEY_NAME

    if gs_key.is_file():
        gs_key.rename(tx_path)
    if drone_key.is_file():
        drone_key.rename(rx_path)

    return str(tx_path), str(rx_path)


def _generate_with_cryptography(output_dir: Path) -> tuple[str, str]:
    """Generate keys using the Python cryptography library (fallback).

    Produces 32-byte random keys compatible with WFB-ng's NaCl encryption.
    This is a fallback when wfb_keygen is not installed.
    """
    tx_path = output_dir / TX_KEY_NAME
    rx_path = output_dir / RX_KEY_NAME

    tx_key = os.urandom(WFB_KEY_SIZE)
    rx_key = os.urandom(WFB_KEY_SIZE)

    # Write key files with restrictive permissions from creation (no race window).
    # os.open with O_CREAT|O_WRONLY and mode 0o600 sets permissions atomically.
    for key_path, key_data in [(tx_path, tx_key), (rx_path, rx_key)]:
        fd = os.open(
            str(key_path),
            os.O_CREAT | os.O_WRONLY | os.O_TRUNC,
            0o600,
        )
        try:
            os.write(fd, key_data)
        finally:
            os.close(fd)

    return str(tx_path), str(rx_path)


def generate_key_pair(output_dir: str | None = None) -> tuple[str, str]:
    """Generate a WFB-ng tx/rx key pair.

    Tries wfb_keygen first (produces fully compatible keys). Falls back to
    Python cryptography library if wfb_keygen is not available.

    Args:
        output_dir: Directory to write keys to. Defaults to /etc/ados/wfb/.

    Returns:
        Tuple of (tx_key_path, rx_key_path).

    Raises:
        OSError: If the output directory cannot be created.
    """
    base = Path(output_dir or DEFAULT_KEY_DIR)
    base.mkdir(parents=True, exist_ok=True)

    # Try wfb_keygen first
    try:
        tx_path, rx_path = _generate_with_wfb_keygen(base)
        log.info("keys_generated", method="wfb_keygen", dir=str(base))
        return tx_path, rx_path
    except FileNotFoundError:
        log.info("wfb_keygen_not_found", fallback="cryptography")
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        log.warning("wfb_keygen_failed", error=str(e), fallback="cryptography")

    # Fallback to Python-generated keys
    tx_path, rx_path = _generate_with_cryptography(base)
    log.info("keys_generated", method="cryptography", dir=str(base))
    return tx_path, rx_path


def get_key_paths(key_dir: str | None = None) -> tuple[str, str]:
    """Get paths to tx.key and rx.key (without checking existence).

    Args:
        key_dir: Key directory. Defaults to /etc/ados/wfb/.

    Returns:
        Tuple of (tx_key_path, rx_key_path).
    """
    base = Path(key_dir or DEFAULT_KEY_DIR)
    return str(base / TX_KEY_NAME), str(base / RX_KEY_NAME)
