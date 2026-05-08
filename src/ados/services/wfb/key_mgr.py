"""WFB-ng encryption key management.

The wire format is libsodium crypto_box: each key file is 64 bytes, the
first 32 a NaCl secret key and the last 32 the matched peer's public
key. `wfb_keygen` produces a paired `gs.key` + `drone.key` pair. The
agent persists the receiver-side bytes at `/etc/ados/wfb/rx.key` and
the transmitter-side bytes at `/etc/ados/wfb/tx.key`. Loading code
treats these as opaque blobs except when computing a public-key
fingerprint for display + cross-rig pair verification.
"""

from __future__ import annotations

import hashlib
import subprocess
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import WFB_KEY_DIR

log = get_logger("wfb.key_mgr")

DEFAULT_KEY_DIR = str(WFB_KEY_DIR)
TX_KEY_NAME = "tx.key"
RX_KEY_NAME = "rx.key"

WFB_KEY_FILE_BYTES = 64
WFB_PUBLIC_HALF_OFFSET = 32


def key_exists(key_dir: str | None = None, role: str | None = None) -> bool:
    """Check if the role-appropriate key file is present.

    Drone profile reads tx.key (used as wfb_tx -K). GS profile reads
    rx.key (used as wfb_rx -K). The bind protocol writes ONE side per
    rig, so requiring both files would make GS rigs (and drone rigs)
    look unpaired forever after a successful bind.

    Without an explicit role, accept either key file as a "paired"
    signal. Callers that know their role should pass it.
    """
    base = Path(key_dir or DEFAULT_KEY_DIR)
    tx_path = base / TX_KEY_NAME
    rx_path = base / RX_KEY_NAME
    if role == "drone":
        return tx_path.is_file()
    if role == "gs":
        return rx_path.is_file()
    return tx_path.is_file() or rx_path.is_file()


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


def read_public_fingerprint(path: str | Path) -> str:
    """Compute a stable 16-hex-char fingerprint of the peer's public key.

    The peer-public half is the second 32 bytes of a 64-byte wfb-ng key
    file. The fingerprint is `blake2b(pub, digest_size=8)` rendered as
    16 lowercase hex characters. Both rigs of a pair compute the same
    fingerprint from their respective key files, so heartbeat
    cross-checks reduce to a string compare.

    Raises:
        FileNotFoundError: If the file does not exist.
        ValueError: If the file is not exactly 64 bytes.
    """
    key_path = Path(path)
    if not key_path.is_file():
        raise FileNotFoundError(f"Key file not found: {path}")

    data = key_path.read_bytes()
    if len(data) != WFB_KEY_FILE_BYTES:
        raise ValueError(
            f"Key file at {path} is {len(data)} bytes, expected "
            f"{WFB_KEY_FILE_BYTES}"
        )

    pub = data[WFB_PUBLIC_HALF_OFFSET:]
    return hashlib.blake2b(pub, digest_size=8).hexdigest()


def generate_key_pair(output_dir: str | None = None) -> tuple[str, str]:
    """Generate a WFB-ng tx/rx key pair via the upstream `wfb_keygen` tool.

    `wfb_keygen` writes `gs.key` and `drone.key` (64 bytes each) into the
    current directory. The function renames them to `tx.key` and
    `rx.key` in `output_dir`.

    Args:
        output_dir: Directory to write keys to. Defaults to /etc/ados/wfb/.

    Returns:
        Tuple of (tx_key_path, rx_key_path).

    Raises:
        FileNotFoundError: If `wfb_keygen` is not on PATH.
        RuntimeError: If `wfb_keygen` exits non-zero or its output is
            missing or the wrong size.
        OSError: If the output directory cannot be created.
    """
    base = Path(output_dir or DEFAULT_KEY_DIR)
    base.mkdir(parents=True, exist_ok=True)

    try:
        result = subprocess.run(
            ["wfb_keygen"],
            capture_output=True,
            text=True,
            timeout=10,
            cwd=str(base),
        )
    except FileNotFoundError as exc:
        log.error("wfb_keygen_not_installed")
        raise FileNotFoundError(
            "wfb_keygen binary not found on PATH. install.sh provisions "
            "wfb-ng with the keygen tool; rerun the installer."
        ) from exc

    if result.returncode != 0:
        raise RuntimeError(f"wfb_keygen failed: {result.stderr.strip()}")

    gs_key = base / "gs.key"
    drone_key = base / "drone.key"
    tx_path = base / TX_KEY_NAME
    rx_path = base / RX_KEY_NAME

    if not gs_key.is_file() or not drone_key.is_file():
        raise RuntimeError(
            "wfb_keygen ran but did not produce both gs.key and drone.key "
            f"in {base}"
        )

    for path in (gs_key, drone_key):
        size = path.stat().st_size
        if size != WFB_KEY_FILE_BYTES:
            raise RuntimeError(
                f"wfb_keygen produced {path.name} of size {size}, "
                f"expected {WFB_KEY_FILE_BYTES}"
            )

    # The agent normalizes everywhere to tx.key / rx.key so the same
    # WfbManager spawn code works on both profiles. The rename is the
    # only step that picks a side; the bytes themselves are
    # role-identified by which half (private + peer-public) is in front.
    gs_key.rename(tx_path)
    drone_key.rename(rx_path)

    log.info("keys_generated", method="wfb_keygen", dir=str(base))
    return str(tx_path), str(rx_path)


def get_key_paths(key_dir: str | None = None) -> tuple[str, str]:
    """Get paths to tx.key and rx.key (without checking existence).

    Args:
        key_dir: Key directory. Defaults to /etc/ados/wfb/.

    Returns:
        Tuple of (tx_key_path, rx_key_path).
    """
    base = Path(key_dir or DEFAULT_KEY_DIR)
    return str(base / TX_KEY_NAME), str(base / RX_KEY_NAME)
