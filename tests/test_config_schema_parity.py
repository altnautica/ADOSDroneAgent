"""CI parity guard: the committed agent-config JSON Schema must match the model.

The schema at ``schemas/agent-config.schema.json`` is generated from the
``ADOSConfig`` Pydantic model by ``scripts/emit_config_schema.py`` and is the
copy the native control surface embeds and serves at ``GET
/api/config/schema``. This guard fails if the committed file drifts from the
model — the same discipline the plugin-manifest schema guard enforces — so a
model change that forgets to regenerate the file is caught in CI rather than
shipping a stale schema.

The emitter runs in a SUBPROCESS, not in-process: it pins the
``ADOS_*_DIR`` base-path env vars before importing ``ados`` so path-derived
defaults come out Linux-FHS-flavoured, and an in-process import here would
see ``ados.core.paths`` already loaded with host-flavoured constants.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[1]
_EMITTER = _ROOT / "scripts" / "emit_config_schema.py"
_SCHEMA = _ROOT / "schemas" / "agent-config.schema.json"


def test_committed_schema_matches_the_model() -> None:
    proc = subprocess.run(
        [sys.executable, str(_EMITTER), "--check"],
        capture_output=True,
        text=True,
        cwd=_ROOT,
        timeout=120,
    )
    assert proc.returncode == 0, (
        "schemas/agent-config.schema.json is out of date with the ADOSConfig "
        f"model; run: python scripts/emit_config_schema.py\n{proc.stderr}"
    )


def test_committed_schema_shape_and_secret_markers() -> None:
    """The committed asset is what a schema-driven UI needs.

    Types + enums + defaults come from the model emit; the secret markers let
    the UI render set/not-set instead of values. Checked on the committed file
    directly (no ados import) so the assertions hold for the exact bytes the
    control surface embeds.
    """
    schema = json.loads(_SCHEMA.read_text(encoding="utf-8"))

    # Every top-level config block is described.
    for block in ("agent", "mavlink", "video", "network", "server", "security", "radio"):
        assert block in schema["properties"], f"missing top-level block {block!r}"

    # Enums survive (the server-mode Literal is the canary).
    mode = schema["$defs"]["ServerConfig"]["properties"]["mode"]
    assert mode["enum"] == ["cloud", "self_hosted", "local"]
    assert mode["default"] == "local"

    # The CRSF RC-lane block carries its full field set with the pinned
    # defaults, so a schema-driven UI renders the lane without hand-typed
    # forms and the enums constrain the writes.
    crsf = schema["$defs"]["CrsfConfig"]["properties"]
    assert crsf["enabled"]["default"] is False
    assert crsf["device"]["default"] is None
    assert crsf["band"]["enum"] == ["dual", "900", "2p4"]
    assert crsf["band"]["default"] == "dual"
    assert crsf["packet_rate_hz"]["default"] == 150
    assert crsf["tx_power_dbm"]["default"] is None
    assert crsf["mode"]["enum"] == ["crsf_rc", "mavlink", "airport"]
    assert crsf["mode"]["default"] == "crsf_rc"
    assert crsf["channel_source"]["enum"] == ["hid", "inject", "hybrid"]
    assert crsf["channel_source"]["default"] == "hid"
    assert crsf["mavlink_transport"]["enum"] == ["serial", "backpack_wifi"]
    assert crsf["mavlink_transport"]["default"] == "serial"
    assert crsf["mavlink_command_enabled"]["default"] is False
    assert crsf["relay_role"]["enum"] == ["none", "repeater", "agent_last_mile"]
    assert crsf["relay_role"]["default"] == "none"

    # Machine-independent defaults: the emitter pins the Linux FHS bases.
    assert (
        schema["$defs"]["TlsConfig"]["properties"]["key_path"]["default"]
        == "/etc/ados/certs/device.key"
    )

    # The full secret set is marked, and nothing else is.
    def _count_markers(node) -> int:
        if isinstance(node, dict):
            own = 1 if node.get("x-secret") is True else 0
            return own + sum(_count_markers(v) for v in node.values())
        if isinstance(node, list):
            return sum(_count_markers(v) for v in node)
        return 0

    assert _count_markers(schema) == 8
    defs = schema["$defs"]
    for def_name, prop in (
        ("TlsConfig", "key_path"),
        ("ApiSecurityConfig", "api_key"),
        ("WireguardConfig", "config_path"),
        ("SelfHostedServerConfig", "api_key"),
        ("SecurityConfig", "hmac_secret"),
        ("ServerConfig", "mqtt_password"),
        ("WifiClientConfig", "password"),
        ("HotspotConfig", "password"),
    ):
        assert defs[def_name]["properties"][prop].get("x-secret") is True, (
            f"{def_name}.{prop} lost its x-secret marker"
        )
