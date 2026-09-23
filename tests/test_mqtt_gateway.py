"""The MQTT gateway's broker credentials, TLS posture and connect retry."""

from __future__ import annotations

import asyncio

import paho.mqtt.client as paho_client
import pytest

from ados.core.config import ADOSConfig
from ados.services.mqtt import gateway as gateway_mod
from ados.services.mqtt.gateway import MqttGateway


class _FakeClient:
    """Records what the gateway asks of the paho client."""

    fail_connects = 0
    tls_error: Exception | None = None
    last: _FakeClient | None = None

    def __init__(self, client_id, callback_api_version, transport):
        self.transport = transport
        self.credentials = None
        self.tls = False
        self.connect_calls = 0
        self.started = False
        self.on_message = None
        self.on_connect = None
        _FakeClient.last = self

    def max_inflight_messages_set(self, n):
        pass

    def ws_set_options(self, path):
        pass

    def username_pw_set(self, username, password=None):
        self.credentials = (username, password)

    def tls_set(self, **kwargs):
        if _FakeClient.tls_error is not None:
            raise _FakeClient.tls_error
        self.tls = True

    def connect(self, host, port, keepalive):
        self.connect_calls += 1
        if self.connect_calls <= _FakeClient.fail_connects:
            raise ConnectionRefusedError("broker down")

    def loop_start(self):
        self.started = True

    def loop_stop(self):
        pass

    def disconnect(self):
        pass

    def publish(self, topic, payload, qos=0):
        pass


class _State:
    """One telemetry pass, then shut the gateway down."""

    armed = False
    snapshot: dict = {}

    def __init__(self, shutdown: asyncio.Event) -> None:
        self._shutdown = shutdown

    def to_dict(self) -> dict:
        self._shutdown.set()
        return {}


@pytest.fixture(autouse=True)
def fake_paho(monkeypatch):
    monkeypatch.setattr(paho_client, "Client", _FakeClient)
    monkeypatch.setattr(gateway_mod, "CONNECT_RETRY_S", 0.01)
    _FakeClient.fail_connects = 0
    _FakeClient.tls_error = None
    _FakeClient.last = None


def _config(mode: str = "cloud", transport: str = "websockets") -> ADOSConfig:
    cfg = ADOSConfig()
    cfg.agent.device_id = "dev1"
    cfg.server.mode = mode
    cfg.server.mqtt_transport = transport
    cfg.server.telemetry_rate = 100
    cfg.server.cloud.mqtt_broker = "broker.example.com"
    cfg.server.self_hosted.mqtt_broker = "broker.example.com"
    return cfg


def _run(cfg: ADOSConfig, api_key: str | None = "pair-key") -> _FakeClient:
    async def go() -> None:
        shutdown = asyncio.Event()
        gw = MqttGateway(cfg, _State(shutdown), api_key=api_key)
        await asyncio.wait_for(gw.run(shutdown), timeout=5)

    asyncio.run(go())
    assert _FakeClient.last is not None
    return _FakeClient.last


def test_default_config_authenticates_as_the_device_id():
    client = _run(_config())
    assert client.credentials == ("dev1", "pair-key")


def test_an_explicit_username_still_wins():
    cfg = _config()
    cfg.server.mqtt_username = "fleet-user"
    assert _run(cfg).credentials == ("fleet-user", "pair-key")


def test_tls_is_applied_on_the_tcp_transport_too():
    client = _run(_config(mode="self_hosted", transport="tcp"))
    assert client.tls is True
    assert client.started is True


def test_only_the_dev_flag_connects_in_plaintext():
    cfg = _config(mode="self_hosted", transport="tcp")
    cfg.server.mqtt_plaintext_dev = True
    client = _run(cfg)
    assert client.tls is False
    assert client.started is True


def test_a_tls_setup_failure_never_connects_in_plaintext():
    _FakeClient.tls_error = ValueError("bad trust store")
    client = _run(_config())
    assert client.connect_calls == 0
    assert client.started is False


def test_a_broker_down_at_boot_is_retried_until_it_answers():
    _FakeClient.fail_connects = 3
    client = _run(_config())
    assert client.connect_calls == 4
    assert client.started is True
