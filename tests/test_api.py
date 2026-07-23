"""Tests for FastAPI REST API routes."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

_SCHEMA_PATH = (
    Path(__file__).resolve().parents[1] / "schemas" / "agent-config.schema.json"
)


@pytest.fixture
def agent_app():
    """Create an API runtime double for testing."""
    return build_api_runtime()


@pytest.fixture
def client(agent_app):
    """FastAPI test client."""
    fastapi_app = create_app(agent_app)
    return TestClient(fastapi_app)


def test_get_setup_status(client):
    resp = client.get("/api/v1/setup/status")
    assert resp.status_code == 200
    data = resp.json()
    assert data["device_id"]
    assert "steps" in data
    assert "access_urls" in data
    assert "mavlink" in data
    assert "video" in data
    # A live agent always reports "configured". Auto-detect commits a
    # profile at install, the operator can override via the webapp at
    # any time, and there is no intermediate "needs review" state.
    assert data["setup_state"] == "configured"
    assert data["profile_source"] in (
        "detected",
        "tiebreaker",
        "override",
        "default",
        "user",
    )
    assert data["profile_suggestion"]["detected"] in ("drone", "ground_station")
    assert data["profile_suggestion"]["source"] in (
        "detected",
        "tiebreaker",
        "override",
        "default",
    )


def test_get_config(client):
    resp = client.get("/api/config")
    assert resp.status_code == 200
    data = resp.json()
    assert "agent" in data
    assert "mavlink" in data


def test_update_config(client):
    resp = client.put("/api/config", json={"key": "agent.name", "value": "new-drone"})
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"


# ─── GET /api/config secret redaction ─────────────────────────────────────────


def _schema_secret_paths() -> list[str]:
    """Every dotted config path the committed schema marks ``x-secret: true``.

    Reads the committed asset and resolves ``$ref`` into ``$defs`` so the set
    is discovered, not hand-listed — a newly marked secret field is picked up
    here (and by the redaction test below) without editing the test. Mirrors
    the ``$ref`` resolution in ``scripts/emit_config_schema.py``.
    """
    schema = json.loads(_SCHEMA_PATH.read_text(encoding="utf-8"))
    defs = schema.get("$defs", {})

    def resolve(node: dict[str, Any]) -> dict[str, Any]:
        seen = 0
        while seen < 20:
            seen += 1
            if "$ref" in node:
                node = defs[node["$ref"].rsplit("/", 1)[-1]]
                continue
            if (
                "allOf" in node
                and len(node["allOf"]) == 1
                and "$ref" in node["allOf"][0]
            ):
                node = node["allOf"][0]
                continue
            return node
        raise AssertionError("$ref chain too deep — cyclic schema?")

    found: list[str] = []

    def walk(node: dict[str, Any], prefix: str) -> None:
        for name, prop in resolve(node).get("properties", {}).items():
            dotted = f"{prefix}{name}"
            if prop.get("x-secret") is True:
                found.append(dotted)
            child = resolve(prop) if ("$ref" in prop or "allOf" in prop) else prop
            if isinstance(child, dict) and child.get("properties"):
                walk(child, dotted + ".")

    walk(schema, "")
    return sorted(found)


def _set_path(cfg: ADOSConfig, dotted: str, value: str) -> None:
    obj: Any = cfg
    parts = dotted.split(".")
    for part in parts[:-1]:
        obj = getattr(obj, part)
    setattr(obj, parts[-1], value)


def _get_path(data: dict[str, Any], dotted: str) -> Any:
    node: Any = data
    for part in dotted.split("."):
        node = node[part]
    return node


def test_get_config_redacts_every_schema_secret():
    """Every field the schema marks ``x-secret: true`` is redacted on GET.

    A canary is planted in each secret field, then GET must return the ``***``
    sentinel for it and the canary must appear nowhere in the response. Because
    the path set is read from the committed schema, this fails the day a new
    secret field is marked but the read surface stops covering it — the
    non-vacuous, drift-proof shape of the schema-parity guard.
    """
    secret_paths = _schema_secret_paths()
    assert secret_paths, "no x-secret paths found in the committed schema"

    cfg = ADOSConfig()
    canaries: dict[str, str] = {}
    for idx, path in enumerate(secret_paths):
        canary = f"LEAK-CANARY-{idx}-do-not-expose"
        _set_path(cfg, path, canary)
        canaries[path] = canary

    resp = TestClient(create_app(build_api_runtime(config=cfg))).get("/api/config")
    assert resp.status_code == 200
    data = resp.json()

    for path, canary in canaries.items():
        assert _get_path(data, path) == "***", f"{path} was not redacted on GET"
        assert canary not in json.dumps(data), f"{path} canary leaked into GET body"


def test_get_config_keeps_non_secret_fields_visible():
    """Redaction touches only the secret paths — a non-secret field (including
    a plain sibling of a secret) round-trips its real value."""
    cfg = ADOSConfig()
    cfg.agent.name = "keep-me-visible"
    cfg.server.mqtt_username = "ados-user-visible"  # sibling of secret mqtt_password
    cfg.network.hotspot.ssid = "VISIBLE-SSID"  # sibling of secret hotspot.password

    resp = TestClient(create_app(build_api_runtime(config=cfg))).get("/api/config")
    data = resp.json()
    assert data["agent"]["name"] == "keep-me-visible"
    assert data["server"]["mqtt_username"] == "ados-user-visible"
    assert data["network"]["hotspot"]["ssid"] == "VISIBLE-SSID"


def test_put_config_rejects_redaction_sentinel_on_every_secret():
    """A PUT of the ``***`` sentinel to any secret path is refused, so a
    GET-then-PUT round trip can never overwrite a real credential with the
    placeholder. Covers the newly included secret paths, not just the original
    four."""
    client = TestClient(create_app(build_api_runtime()))
    for path in _schema_secret_paths():
        resp = client.put("/api/config", json={"key": path, "value": "***"})
        assert resp.status_code == 400, f"{path} accepted the redaction sentinel"
        assert resp.json()["detail"]["error"]["code"] == "E_REDACTED_SENTINEL"


def test_get_logs_degrades_when_store_unreachable(client):
    """With no logging store socket present (the test environment), the legacy
    /api/logs endpoint degrades to an empty list with a warning rather than a
    500: losing history degrades debugging, not flight."""
    resp = client.get("/api/logs")
    assert resp.status_code == 200
    data = resp.json()
    assert data["entries"] == []
    assert data["total"] == 0
    assert "warning" in data


def test_legacy_entry_timestamp_is_iso_string():
    """The legacy entry mapping must emit an ISO-8601 string timestamp, not a
    float. A numeric timestamp breaks consumers that slice/parse the field as a
    string (the live dashboard log view does exactly that)."""
    from datetime import datetime

    from ados.api.routes.logs import _legacy_entry

    entry = _legacy_entry(
        {
            "id": 7,
            "ts_us": 1_700_000_000_000_000,
            "source": "python-agent",
            "level": "info",
            "target": "ados.test",
            "msg": "hello",
        }
    )
    assert isinstance(entry["timestamp"], str)
    # Round-trips through the ISO parser without raising.
    datetime.fromisoformat(entry["timestamp"])
    # The legacy consumer expects an upper-case level and the logger name.
    assert entry["level"] == "INFO"
    assert entry["logger"] == "ados.test"
    assert entry["message"] == "hello"


