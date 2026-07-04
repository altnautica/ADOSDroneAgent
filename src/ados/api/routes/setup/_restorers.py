"""Per-section restore helpers used by the apply rollback path."""

from __future__ import annotations


def restore_profile(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    agent = getattr(config, "agent", None)
    if agent is not None and "profile" in snap:
        agent.profile = str(snap.get("profile") or "")
    ground = getattr(config, "ground_station", None)
    if ground is not None and "ground_role" in snap:
        prior = str(snap.get("ground_role") or "")
        if prior:
            ground.role = prior  # type: ignore[assignment]


def restore_cloud(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    server = getattr(config, "server", None)
    if server is None:
        return
    if "mode" in snap:
        prior = str(snap.get("mode") or "cloud")
        if prior in ("cloud", "self_hosted", "local"):
            server.mode = prior  # type: ignore[assignment]
    sh = getattr(server, "self_hosted", None)
    if sh is not None:
        if "self_hosted_url" in snap:
            sh.url = str(snap.get("self_hosted_url") or "")
        if "self_hosted_mqtt_broker" in snap:
            sh.mqtt_broker = str(snap.get("self_hosted_mqtt_broker") or "")
        if "self_hosted_mqtt_port" in snap:
            try:
                sh.mqtt_port = int(snap.get("self_hosted_mqtt_port") or 0)
            except (TypeError, ValueError):
                pass


def restore_network(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    net = getattr(config, "network", None)
    if net is None:
        return
    wifi = getattr(net, "wifi_client", None)
    if wifi is not None:
        if "wifi_ssid" in snap:
            wifi.ssid = str(snap.get("wifi_ssid") or "")
        if "wifi_password" in snap:
            wifi.password = str(snap.get("wifi_password") or "")
    hotspot = getattr(net, "hotspot", None)
    if hotspot is not None and "hotspot_enabled" in snap:
        hotspot.enabled = bool(snap.get("hotspot_enabled"))


def restore_ui(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    ui = getattr(config, "ui", None)
    if ui is None:
        return
    if "theme" in snap:
        prior = str(snap.get("theme") or "")
        if prior in ("dark", "light"):
            ui.theme = prior  # type: ignore[assignment]


def restore_advanced(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    logging_cfg = getattr(config, "logging", None)
    if logging_cfg is not None and "log_level" in snap:
        prior = str(snap.get("log_level") or "")
        if prior:
            logging_cfg.level = prior


def restore_regulatory(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    net = getattr(config, "network", None)
    reg = getattr(net, "regulatory", None) if net is not None else None
    if reg is None:
        return
    if "mode" in snap:
        prior = str(snap.get("mode") or "")
        if prior in ("unrestricted", "region"):
            reg.mode = prior  # type: ignore[assignment]
    if "region" in snap:
        prior_region = snap.get("region")
        reg.region = str(prior_region) if isinstance(prior_region, str) else None
    if "ack_operator" in snap:
        prior_op = snap.get("ack_operator")
        reg.ack_operator = str(prior_op) if isinstance(prior_op, str) else None
    if "ack_at" in snap:
        prior_at = snap.get("ack_at")
        reg.ack_at = str(prior_at) if isinstance(prior_at, str) else None


def restore_wfb(runtime, snap: dict[str, object]) -> None:
    config = runtime.config
    video = getattr(config, "video", None)
    wfb = getattr(video, "wfb", None) if video is not None else None
    if wfb is None:
        return
    if "channel" in snap:
        try:
            wfb.channel = int(snap.get("channel") or 0)
        except (TypeError, ValueError):
            pass
    if "tx_power_dbm" in snap:
        try:
            wfb.tx_power_dbm = int(snap.get("tx_power_dbm") or 0)
        except (TypeError, ValueError):
            pass
    if "mcs_index" in snap:
        try:
            wfb.mcs_index = int(snap.get("mcs_index") or 0)
        except (TypeError, ValueError):
            pass
    if "topology" in snap:
        prior = str(snap.get("topology") or "")
        if prior in ("host_vbus", "powered_hub", "external_5v"):
            wfb.topology = prior  # type: ignore[assignment]
