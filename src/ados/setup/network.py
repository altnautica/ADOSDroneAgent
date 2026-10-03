"""Network section setter for the batch-apply route.

Config-write-only: this setter persists the operator's WiFi-client and
hotspot preferences onto ``runtime.config.network`` and saves them to
``/etc/ados/config.yaml``. Applying them to the live interfaces is owned
elsewhere — the WiFi client join/leave is driven by the native network
daemon (``/network/client/*`` over the WiFi command socket), and the
hotspot is brought up by the ground-station hostapd service, which reads
``network.hotspot.enabled`` at start. A change here therefore takes effect
the next time the relevant network service (re)starts.

A failed persist is surfaced (``ok=False``), never swallowed: a save that
did not reach disk must not be reported as success. Each field is optional
and a ``None`` payload short-circuits to a no-op success so the apply route
can pass through sections the caller did not modify.
"""

from __future__ import annotations

from typing import Any

from ados.setup._persist import persist_config
from ados.setup.models import NetworkApplyRequest, SetupActionResult


def apply_network(
    runtime: Any,
    request: NetworkApplyRequest | None,
) -> SetupActionResult:
    """Persist a network slice update onto ``runtime.config.network``.

    Returns ``ok=True`` even when the request is empty so the batch apply
    route can iterate sections without special-casing absent payloads.
    ``wifi_password`` is recorded but never echoed back through ``data``.
    A save failure returns ``ok=False``.
    """
    if request is None:
        return SetupActionResult(
            ok=True,
            message="No network changes requested.",
            data={"changed": False},
        )

    config = runtime.config
    network = getattr(config, "network", None)
    if network is None:
        return SetupActionResult(
            ok=False,
            message="Network configuration is not available on this agent.",
        )

    values: dict[str, object] = {}
    changed_fields: list[str] = []

    if request.wifi_ssid is not None or request.wifi_password is not None:
        wifi = getattr(network, "wifi_client", None)
        if wifi is None:
            return SetupActionResult(
                ok=False,
                message="WiFi client configuration is not available.",
            )
        if request.wifi_ssid is not None:
            ssid = str(request.wifi_ssid).strip()
            if wifi.ssid != ssid:
                values["network.wifi_client.ssid"] = ssid
                changed_fields.append("wifi_ssid")
        if request.wifi_password is not None:
            password = str(request.wifi_password)
            if wifi.password != password:
                values["network.wifi_client.password"] = password
                changed_fields.append("wifi_password")

    if request.hotspot_enabled is not None:
        hotspot = getattr(network, "hotspot", None)
        if hotspot is None:
            return SetupActionResult(
                ok=False,
                message="Hotspot configuration is not available.",
            )
        flag = bool(request.hotspot_enabled)
        if hotspot.enabled != flag:
            values["network.hotspot.enabled"] = flag
            changed_fields.append("hotspot_enabled")

    # Surface a failed persist rather than swallowing it: a change that did
    # not reach /etc/ados/config.yaml must not be reported as success.
    failed = persist_config(runtime, values, what="Network settings")
    if failed is not None:
        return failed

    data: dict[str, object] = {
        "changed": bool(changed_fields),
        "fields": changed_fields,
    }
    if changed_fields:
        message = (
            f"Network updated ({', '.join(changed_fields)}); "
            "applies on the next network service restart."
        )
    else:
        message = "No network changes detected."
    return SetupActionResult(ok=True, message=message, data=data)
