"""Operating-region (regulatory posture) step of the onboarding wizard."""

from __future__ import annotations

from typing import Any

from ados.setup._persist import persist_config
from ados.setup.models import RegulatoryApplyRequest, SetupActionResult


def _is_valid_region_code(code: str) -> bool:
    """A region code is an uppercase 2-character A-Z ISO 3166-1 alpha-2.

    Mirrors the shape ``set_regulatory_domain`` validates against (two
    chars) but is stricter (letters only) because an operating region is
    always a country code, never a numeric private regdb entry.
    """
    return len(code) == 2 and code.isalpha() and code.isupper()


def apply_regulatory(
    runtime: Any,
    request: RegulatoryApplyRequest | None,
) -> SetupActionResult:
    """Persist the operating-region posture onto ``config.network.regulatory``.

    ``mode`` is ``unrestricted`` (radiate out of the box; the operator is
    responsible for local RF compliance) or ``region`` (pin an operating
    region and apply its channel/power limits). ``region`` is required and
    validated as an uppercase ISO 3166-1 alpha-2 code when ``mode`` is
    ``region``; it is cleared to ``None`` when ``mode`` is ``unrestricted``.
    An operator id + ISO-8601 timestamp are recorded for the audit trail.

    The radio re-reads the posture on its next restart, so a real change
    is surfaced as ``restart_required`` to the caller. This setter only
    persists config; it never touches the live link.

    Returns ``ok=True`` on a no-op so the batch-apply route can iterate
    sections without special-casing an absent payload.
    """
    if request is None:
        return SetupActionResult(
            ok=True,
            message="No region changes requested.",
            data={"changed": False},
        )

    config = runtime.config
    net = getattr(config, "network", None)
    reg = getattr(net, "regulatory", None) if net is not None else None
    if reg is None:
        return SetupActionResult(
            ok=False,
            message="Regulatory configuration is not available on this agent.",
        )

    # Resolve the target mode. When the caller sends a region but no
    # explicit mode, treat it as a region pin so the webapp can send the
    # country code alone.
    target_mode = request.mode
    if target_mode is None:
        if request.region:
            target_mode = "region"
        else:
            return SetupActionResult(
                ok=True,
                message="No region changes detected.",
                data={"changed": False},
            )
    if target_mode not in ("unrestricted", "region"):
        return SetupActionResult(
            ok=False,
            message="mode must be 'unrestricted' or 'region'.",
        )

    target_region: str | None
    if target_mode == "region":
        raw = (request.region or "").strip().upper()
        if not raw:
            return SetupActionResult(
                ok=False,
                message="region is required when pinning a region.",
            )
        if not _is_valid_region_code(raw):
            return SetupActionResult(
                ok=False,
                message=(
                    "region must be a 2-letter country code (e.g. US, DE, GB), "
                    f"got '{request.region}'."
                ),
            )
        target_region = raw
    else:
        target_region = None

    previous_mode = str(getattr(reg, "mode", "unrestricted") or "unrestricted")
    previous_region = getattr(reg, "region", None)

    changed = previous_mode != target_mode or previous_region != target_region

    if changed:
        # Record who made the choice and when, for the audit trail. The
        # operator id falls back to the device name when no explicit
        # operator is supplied (the webapp/CLI run unattended on-box).
        operator = str(getattr(config.agent, "name", "") or "") or "operator"
        from ados.api.routes.setup._common import now_iso

        ack_at = now_iso()
        failed = persist_config(
            runtime,
            {
                "network.regulatory.mode": target_mode,
                "network.regulatory.region": target_region,
                "network.regulatory.ack_operator": operator,
                "network.regulatory.ack_at": ack_at,
            },
            what="Operating region",
        )
        if failed is not None:
            return failed

        # The audit trail: a regulatory posture is a decision that outlives the
        # request, and the operator + timestamp recorded above are exactly what
        # an after-the-fact "who set this region" question needs.
        from ados.core import audit

        audit.record(
            audit.REGULATORY_POSTURE_APPLIED,
            audit.ACTOR_OPERATOR,
            {
                "mode": target_mode,
                "region": target_region,
                "previous_mode": previous_mode,
                "previous_region": previous_region,
                "ack_operator": operator,
                "ack_at": ack_at,
                "restart_required": True,
            },
        )

    data: dict[str, object] = {
        "changed": changed,
        "mode": target_mode,
        "region": target_region or "",
    }
    if changed:
        data["restart_required"] = True

    if target_mode == "unrestricted":
        message = "Operating region set to unrestricted."
    else:
        message = f"Operating region pinned to {target_region}."
    if changed:
        message += " Restart the agent to apply."
    return SetupActionResult(ok=True, message=message, data=data)
