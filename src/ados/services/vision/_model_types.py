"""Vision model manager types and pure helpers: download state, registry
metadata, model-ref resolution, board family mapping, registry auth."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import Any, Protocol, runtime_checkable
from urllib.parse import urlsplit

from ados.core.logging import get_logger

log = get_logger("vision-models")


class DownloadState(StrEnum):
    """Model download lifecycle states."""

    IDLE = "idle"
    DOWNLOADING = "downloading"
    COMPLETED = "completed"
    FAILED = "failed"


@dataclass
class DownloadProgress:
    """Snapshot of a model download."""

    state: DownloadState = DownloadState.IDLE
    bytes_downloaded: int = 0
    total_bytes: int = 0
    speed_bps: float = 0.0
    eta_seconds: float = 0.0
    error: str | None = None

    def percent(self) -> float:
        if self.total_bytes <= 0:
            return 0.0
        return min(100.0, (self.bytes_downloaded / self.total_bytes) * 100.0)

    def to_dict(self) -> dict[str, Any]:
        return {
            "state": self.state.value,
            "percent": round(self.percent(), 1),
            "bytes_downloaded": self.bytes_downloaded,
            "total_bytes": self.total_bytes,
            "speed_bps": round(self.speed_bps),
            "eta_seconds": round(self.eta_seconds),
            "error": self.error,
        }


@dataclass
class ModelInfo:
    """Metadata for a single model from the registry."""

    id: str
    name: str = ""
    description: str = ""
    task: str = ""  # detection, tracking, depth, segmentation
    variants: list[dict[str, Any]] = field(default_factory=list)
    # True when the registry marks this the recommended default for its task, so
    # auto-provisioning can pick a sensible detector for a fresh NPU board without
    # an operator choosing one. Absent in the registry ⇒ False (nothing is a
    # default unless the registry says so).
    recommended: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "task": self.task,
            "variants": self.variants,
            "recommended": self.recommended,
        }


@runtime_checkable
class ModelRefLike(Protocol):
    """Structural type for a manifest VisionModelRef — kept decoupled (no plugin import here)."""

    id: str
    runtime: str
    board_match: str
    sha256: str | None
    source: str | None
    path: str | None


class ResolutionState(StrEnum):
    RESOLVED = "resolved"            # a verified model file is available at .path
    NEEDS_MODEL = "needs_model"      # not cached + not fetchable → sideload (carries a reason)
    VERIFY_FAILED = "verify_failed"  # fetched but sha256 != the pinned digest


@dataclass
class ModelResolution:
    """Outcome of resolving a plugin's model reference (the heartbeat needs-model surface)."""

    state: str
    model_id: str
    runtime: str | None = None
    path: str | None = None
    reason: str | None = None

    @property
    def ok(self) -> bool:
        return self.state == ResolutionState.RESOLVED

    def to_dict(self) -> dict[str, Any]:
        return {"state": self.state, "model_id": self.model_id, "runtime": self.runtime,
                "path": self.path, "reason": self.reason}


def sha256_file(path: str | Path, chunk: int = 1 << 20) -> str:
    """Hex SHA-256 of a file, streamed."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(chunk), b""):
            h.update(block)
    return h.hexdigest()


def board_family(board_id: str | None) -> str:
    """Map a board identifier to the model ``board_match`` family (raspberrypi-5 | rk3588 | orin | cpu | generic).

    Substring-matched + case-insensitive so a SoC string ("RK3582"), a board slug
    ("rock-5c-lite") or a display name ("Radxa ROCK 5C Lite (RK3582)") all resolve. The
    RK3588 family covers the rk3588/rk3582 NPU-class siblings (the small rk356x NPU is NOT
    in this family — it falls through to generic).

    The A733 (Radxa Cubie A7S, and its A7Z sibling on the same SoC) carries a Vivante
    VIP9000 NPU with no in-tree ados-vision backend (see cubie-a7s.yaml's
    ``compute.npu_tops`` comment), so it is deliberately NOT routed into an accelerator
    family; it resolves to ``cpu``, the CPU-ONNX ``board_match`` token every plugin's
    ``board_variants`` catalog actually declares for its non-accelerator model (see
    ``ADOSExtensions/extensions/follow-me/manifest.yaml``'s ``coco-person`` model:
    ``board_match: rk3588``/``runtime: rknn`` + ``board_match: cpu``/``runtime: onnx`` — no
    manifest declares a bare ``"generic"`` entry). Falling through to the bare ``generic``
    default here would match no declared variant, so ``resolve_model_ref`` would silently
    pick ``refs[0]`` — the wrong, accelerator-only model this board cannot run — instead of
    resolving the CPU model."""
    b = (board_id or "").lower()
    if any(k in b for k in ("orin", "tegra", "jetson")):
        return "orin"
    if any(
        k in b
        for k in (
            "rk3588", "rk3582", "orange-pi-5", "orange pi 5", "orangepi5", "orangepi-5",
            "rock-5c", "rock5c",
        )
    ):
        return "rk3588"
    if any(
        k in b
        for k in (
            "raspberrypi-5", "raspberry pi 5", "raspberrypi5", "bcm2712",
            "raspberry pi cm5", "raspberry pi compute module 5", "rpi-cm5",
        )
    ):
        # Raspberry Pi 5 (BCM2712). A plugin that ships an accelerator variant keyed to this
        # family expects the Pi 5's add-on AI HAT (Hailo-class); a bare Pi 5 with no HAT should
        # still declare — and will then match — the plugin's cpu/generic fallback via the second
        # pass in ``select_ref_for_board``. (Presence-gating the accelerator on measured NPU TOPS
        # is the follow-up refinement; this restores the family the board was silently missing.)
        return "raspberrypi-5"
    if any(k in b for k in ("a733", "sun60i", "cubie-a7s", "cubie a7s", "cubie-a7z", "cubie a7z")):
        return "cpu"
    return "generic"


# Optional per-host credentials for a private model registry. The agent's open registry
# path is unauthenticated and a pre-signed URL carries its own credential, so this is only
# consulted when a source host needs a header (bearer/basic). Tokens are NEVER logged.
_REGISTRY_AUTH_PATH = Path("/etc/ados/model-registry-auth.json")


def registry_auth_headers(source: str, *, creds_path: Path = _REGISTRY_AUTH_PATH) -> dict[str, str]:
    """Auth headers for a by-reference model fetch, keyed by the source host.

    ``/etc/ados/model-registry-auth.json`` maps a host to either a token string (sent as
    ``Authorization: Bearer <token>`` unless it already names a scheme) or an object
    ``{"header": ..., "value": ...}`` for a custom header. A signed-URL source needs no entry.
    Missing/unreadable/unmatched ⇒ no headers (the fetch proceeds unauthenticated)."""
    try:
        host = urlsplit(source).hostname or ""
    except ValueError:
        return {}
    if not host:
        return {}
    try:
        creds = json.loads(creds_path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}
    entry = creds.get(host) if isinstance(creds, dict) else None
    if isinstance(entry, str) and entry:
        scheme_named = entry.lower().startswith(("bearer ", "basic ", "token "))
        return {"Authorization": entry if scheme_named else f"Bearer {entry}"}
    if isinstance(entry, dict):
        value = entry.get("value")
        if value:
            return {str(entry.get("header") or "Authorization"): str(value)}
    return {}
