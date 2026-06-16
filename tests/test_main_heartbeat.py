"""Direct tests for AgentApp._build_heartbeat_payload.

The native Rust cloud relay is the production heartbeat path; this
module exercises the parallel in-process builder in `core/main.py`,
which serves the same heartbeat contract from inside the API process.
"""

from __future__ import annotations

from ados.core.config import ADOSConfig
from ados.core.main import AgentApp


def _fresh_app() -> AgentApp:
    """Build an AgentApp without running .start() (no asyncio loop)."""
    config = ADOSConfig()
    app = AgentApp(config)
    return app


def test_heartbeat_payload_includes_runtime_mode() -> None:
    """The heartbeat carries the native-vs-packaged aggregate so the GCS
    can light the per-node runtime badge. On the test box (no native
    binaries installed) the aggregate resolves to 'packaged'."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["runtimeMode"] in ("native", "hybrid", "packaged")


def test_heartbeat_payload_video_restart_attempts_default() -> None:
    """No video pipeline attached → counter reads 0 (not absent)."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["videoRestartAttempts"] == 0


def test_heartbeat_payload_video_restart_attempts_reflected() -> None:
    """A pipeline that exposes restart_attempts() shows up on the wire."""
    app = _fresh_app()

    class FakePipeline:
        def restart_attempts(self) -> int:
            return 3

    app._video_pipeline = FakePipeline()
    payload = app._build_heartbeat_payload()
    assert payload["videoRestartAttempts"] == 3


def test_heartbeat_payload_mavlink_ws_url_prev_on_rotation() -> None:
    """Previous URL appears for one tick when the configured value changes."""
    app = _fresh_app()
    remote = app.config.remote_access.cloudflare

    # First tick: original URL, no prev.
    remote.mavlink_ws_url = "wss://example.invalid/mavlink-1"
    first = app._build_heartbeat_payload()
    assert first["mavlinkWsUrl"] == "wss://example.invalid/mavlink-1"
    assert "mavlinkWsUrlPrev" not in first

    # Second tick: rotated URL, prev surfaces.
    remote.mavlink_ws_url = "wss://example.invalid/mavlink-2"
    second = app._build_heartbeat_payload()
    assert second["mavlinkWsUrl"] == "wss://example.invalid/mavlink-2"
    assert second["mavlinkWsUrlPrev"] == "wss://example.invalid/mavlink-1"

    # Third tick: same URL, prev gone.
    third = app._build_heartbeat_payload()
    assert third["mavlinkWsUrl"] == "wss://example.invalid/mavlink-2"
    assert "mavlinkWsUrlPrev" not in third


def test_heartbeat_payload_mavlink_ws_url_prev_absent_when_unset() -> None:
    """No URL configured → tracker stays None and prev never appears."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert "mavlinkWsUrl" not in payload
    assert "mavlinkWsUrlPrev" not in payload


class _FakeWfb:
    def __init__(self, status: dict) -> None:
        self._status = status

    def get_status(self) -> dict:
        return self._status


def test_heartbeat_payload_wfb_adapter_verdict_absent_defaults() -> None:
    """No wfb manager → chipset null, injectionOk false (never absent)."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["wfbAdapterChipset"] is None
    assert payload["wfbAdapterInjectionOk"] is False


def test_heartbeat_payload_wfb_adapter_verdict_reflected() -> None:
    """A verified RTL radio is hoisted to the payload root."""
    app = _fresh_app()
    app._wfb_manager = _FakeWfb(
        {
            "state": "connected",
            "interface": "wlan1",
            "adapter_chipset": "RTL8812EU",
            "adapter_injection_ok": True,
        }
    )
    payload = app._build_heartbeat_payload()
    assert payload["wfbAdapterChipset"] == "RTL8812EU"
    assert payload["wfbAdapterInjectionOk"] is True
    # And it also rides inside the radio block.
    assert payload["radio"]["adapter_chipset"] == "RTL8812EU"
    assert payload["radio"]["adapter_injection_ok"] is True


def test_heartbeat_payload_wfb_no_injection_adapter_is_loud() -> None:
    """No injection radio found → injectionOk false at the payload root."""
    app = _fresh_app()
    app._wfb_manager = _FakeWfb(
        {
            "state": "disconnected",
            "interface": "",
            "adapter_chipset": None,
            "adapter_injection_ok": False,
        }
    )
    payload = app._build_heartbeat_payload()
    assert payload["wfbAdapterChipset"] is None
    assert payload["wfbAdapterInjectionOk"] is False


def test_heartbeat_payload_radio_stack_state_present() -> None:
    """The heartbeat carries a radio-stack verdict from the known set."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["radioStackState"] in (
        "ok",
        "no_injection",
        "unpaired",
        "no_bind_artifacts",
        "stack_incomplete",
    )


def test_heartbeat_payload_wfb_failover_state_default_local() -> None:
    """No failover sidecar on the test box → the heartbeat reads 'local'."""
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["wfbFailoverState"] == "local"


def test_heartbeat_payload_wfb_failover_state_reflects_sidecar(monkeypatch) -> None:
    """A cloud_relay failover sidecar surfaces on the cloud heartbeat."""
    import ados.core.radio_block as radio_block

    monkeypatch.setattr(
        radio_block, "read_wfb_failover_state", lambda: "cloud_relay"
    )
    app = _fresh_app()
    payload = app._build_heartbeat_payload()
    assert payload["wfbFailoverState"] == "cloud_relay"


def test_heartbeat_payload_advertises_only_the_raw_mavlink_ws(monkeypatch) -> None:
    """The MAVLink WS is authenticated on the raw ``mavlinkWs`` proxy via a
    ticket subprotocol the native router validates — there is no separate gated
    endpoint, so no profile advertises ``mavlinkWsAuthenticated``."""
    import ados.core.main.heartbeat_payload as hb

    monkeypatch.setattr(hb, "_get_local_ip", lambda: "10.0.0.5")
    for profile in ("ground_station", "drone"):
        app = _fresh_app()
        app.config.agent.profile = profile
        urls = app._build_heartbeat_payload()["manualConnectionUrls"]
        assert "mavlinkWsAuthenticated" not in urls
        assert urls["mavlinkWs"] == "ws://10.0.0.5:8765/"


def test_heartbeat_payload_radio_churn_fields_ride_block() -> None:
    """tx_zombie_kills / tx_bytes_per_s / restart_count from the live status
    reach the radio block on the assembled payload."""
    app = _fresh_app()
    app._wfb_manager = _FakeWfb(
        {
            "state": "connected",
            "interface": "wlan1",
            "tx_zombie_kills": 4,
            "tx_bytes_per_s": 512000.0,
            "restart_count": 1,
        }
    )
    payload = app._build_heartbeat_payload()
    assert payload["radio"]["tx_zombie_kills"] == 4
    assert payload["radio"]["tx_bytes_per_s"] == 512000.0
    assert payload["radio"]["restart_count"] == 1
