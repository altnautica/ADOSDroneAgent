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

    # Surface a failed persist rather than swallowing it: a change that did
    # not reach /etc/ados/config.yaml must not be reported as success.
    if changed_fields:
        saver = getattr(getattr(runtime, "raw_runtime", None), "save_config", None)
        if callable(saver):
            try:
                persisted = bool(saver())
            except Exception as exc:  # noqa: BLE001 (surface, don't swallow)
                return SetupActionResult(
                    ok=False,
                    message=f"Network settings not saved: config write failed: {exc}",
                )
            if not persisted:
                return SetupActionResult(
                    ok=False,
                    message="Network settings not saved: config could not be written to disk.",
                )

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
