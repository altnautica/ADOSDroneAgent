"""The board fingerprint sidecar has ONE writer, and the two languages agree on
its shape.

``/run/ados/board.json`` carries the board's identity, tier and NPU capability.
Every Rust reader keys on it: the pairing route's ``board`` field, the native
status route's board object, and the cloud offload reconciler's ``npu_tops``
(which decides local-vs-offload detection).

It used to be written only by Python, from inside the FastAPI runtime, so on the
advertised zero-Python headless profile it was never written at all — board
``unknown`` and ``npu_tops: 0`` on a board with a 6-TOPS NPU. The writer now
lives in the supervisor (``crates/ados-hal-probe/src/board_sidecar.rs``) and is
the only one. ``BoardInfo.to_dict()`` remains the Python-side declaration of the
document's field set, so these tests hold the two ends together: a field added
on one side without the other is caught here rather than on a rig.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from ados.hal.detect import BoardInfo

REPO_ROOT = Path(__file__).resolve().parents[1]
RUST_WRITER = REPO_ROOT / "crates" / "ados-hal-probe" / "src" / "board_sidecar.rs"

pytestmark = pytest.mark.skipif(
    not RUST_WRITER.exists(),
    reason="native crate not in this checkout",
)


def _rust_fingerprint_fields() -> list[str]:
    """The `BoardFingerprint` struct's serialized field names, in source order."""
    src = RUST_WRITER.read_text()
    match = re.search(
        r"pub struct BoardFingerprint \{(.*?)\n\}", src, re.DOTALL
    )
    assert match, "BoardFingerprint must be a struct in the Rust writer"
    return re.findall(r"^\s*pub ([a-z_]+):", match.group(1), re.MULTILINE)


def test_the_rust_writer_carries_the_python_field_set_plus_version():
    board = BoardInfo(
        name="Radxa ROCK 5C Lite (RK3582)",
        model="rock-5c-lite",
        tier=4,
        ram_mb=16000,
        cpu_cores=8,
        npu_tops=6.0,
    )
    python_fields = set(board.to_dict())
    rust_fields = set(_rust_fingerprint_fields())

    # `version` is the one key the Rust document adds: the sidecar registry
    # declares the board sidecar at version 1, and the Python writer omitted it,
    # so every Rust reader logged a schema-version warning on every boot.
    assert rust_fields == python_fields | {"version"}, (
        "the board sidecar's two schema declarations have drifted: "
        f"only in Rust={sorted(rust_fields - python_fields - {'version'})}, "
        f"only in Python={sorted(python_fields - rust_fields)}"
    )


def test_the_declared_document_reports_a_real_accelerator():
    # The field that made an NPU board offload work it could run onboard.
    npu = BoardInfo(
        name="Radxa ROCK 5C Lite (RK3582)",
        model="rock-5c-lite",
        tier=4,
        ram_mb=16000,
        cpu_cores=8,
        npu_tops=6.0,
    ).to_dict()
    assert npu["npu_tops"] == 6.0
    assert npu["has_accelerator"] is True

    plain = BoardInfo(
        name="Raspberry Pi 4 Model B",
        model="rpi4b",
        tier=3,
        ram_mb=4096,
        cpu_cores=4,
    ).to_dict()
    assert plain["npu_tops"] == 0.0
    assert plain["has_accelerator"] is False


def test_python_no_longer_writes_the_sidecar():
    """One document, one writer.

    A second writer of the same file is what let the Python-only writer look
    sufficient while the zero-Python profile had none, so the Python side must
    not grow one back. Mentions of the path in comments and the generated
    contract table are fine; a WRITE through `BOARD_JSON` is not.
    """
    import ados.hal.detect as detect

    assert not hasattr(detect, "persist_board_sidecar")

    write_calls = re.compile(
        r"BOARD_JSON[^\n]*\.(write_text|write_bytes|open)\("
        r"|\.replace\(\s*BOARD_JSON\s*\)"
    )
    offenders = [
        str(p.relative_to(REPO_ROOT))
        for p in (REPO_ROOT / "src" / "ados").rglob("*.py")
        if write_calls.search(p.read_text())
    ]
    assert offenders == [], f"unexpected Python board.json writer: {offenders}"
