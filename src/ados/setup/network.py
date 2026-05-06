"""Network section setter for the batch-apply route.

Stub implementation that records the operator's intent on the live
config object so the rest of the setup flow can read it back. Real
wiring of the WiFi client + hotspot toggle into the network services
runs through the existing ground-station network module and is wired
in a later iteration.

The setter is intentionally permissive: each field is optional, and a
None payload short-circuits to a no-op success result so the apply
route can pass through sections the caller did not modify.
"""

from __future__ import annotations

from typing import Any

from ados.setup.models import NetworkApplyRequest, SetupActionResult


def apply_network(
    runtime: Any,
    request: NetworkApplyRequest | None,
) -> SetupActionResult:
    """Persist a network slice update onto ``runtime.config.network``.

    Returns ``ok=True`` even when the request is empty so the batch
    apply route can iterate sections without special-casing absent
    payloads. ``wifi_password`` is recorded but never echoed back
    through ``data``.
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

    changed_fields: list[str] = []

    if request.wifi_ssid is not None:
        wifi = getattr(network, "wifi_client", None)
        if wifi is None:
            return SetupActionResult(
                ok=False,
                message="WiFi client configuration is not available.",
            )
        ssid = str(request.wifi_ssid).strip()
        if wifi.ssid != ssid:
            wifi.ssid = ssid
            changed_fields.append("wifi_ssid")

    if request.wifi_password is not None:
        wifi = getattr(network, "wifi_client", None)
        if wifi is None:
            return SetupActionResult(
                ok=False,
                message="WiFi client configuration is not available.",
            )
        password = str(request.wifi_password)
        if wifi.password != password:
            wifi.password = password
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
            hotspot.enabled = flag
            changed_fields.append("hotspot_enabled")

    saver = getattr(getattr(runtime, "raw_runtime", None), "save_config", None)
    if changed_fields and callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "changed": bool(changed_fields),
        "fields": changed_fields,
    }
    if changed_fields:
        message = f"Network updated ({', '.join(changed_fields)})."
    else:
        message = "No network changes detected."
    return SetupActionResult(ok=True, message=message, data=data)
