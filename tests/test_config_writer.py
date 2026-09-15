"""The one config writer: merge semantics, ownership and atomicity.

``/etc/ados/config.yaml`` is co-owned. The Python models describe part of it;
``ados-radio``, ``ados-supervisor``, ``ados-plugin-host`` and the installer's
watchdog step read keys those models have never heard of. The write path used
to serialise the whole Pydantic model over the file, which deleted every one of
those keys and froze that release's defaults into the node's document.

These tests pin the three properties that stops:

* a key the model does not declare round-trips verbatim;
* only the leaves a caller actually changed are written, so an untouched
  default stays absent and keeps tracking the shipped value;
* a write that cannot land leaves the previous document byte-intact.
"""

from __future__ import annotations

import fcntl
import os
from pathlib import Path

import pytest
import yaml

from ados.core.config import ADOSConfig, load_config
from ados.core.config.writer import (
    ConfigWriteResult,
    merge_into_config,
    persist_config_model,
    set_config_values,
    update_config,
)

# A document that exercises both halves of the co-ownership problem: keys only
# the Rust side reads (`mavlink.injector_arbitration` arms the FC-write
# arbiter, `network.watchdog.*` arms the SoC watchdog, `agent.headless`
# selects the zero-Python flight profile, `video.wfb.reg_gate_strict` is the
# documented escape hatch for an EEPROM-locked dongle) sitting alongside
# Python-owned sections with populated siblings.
_CO_OWNED = """\
agent:
  device_id: abc12345
  name: bench-node
  headless: true
mavlink:
  injector_arbitration: true
  system_id: 42
video:
  camera:
    width: 1920
    hflip: true
  wfb:
    reg_gate_strict: false
    channel: 149
network:
  watchdog:
    enabled: true
    timeout_s: 30
"""


def _write_doc(tmp_path: Path, body: str = _CO_OWNED) -> Path:
    cfg = tmp_path / "config.yaml"
    cfg.write_text(body, encoding="utf-8")
    return cfg


def _read(cfg: Path) -> dict:
    return yaml.safe_load(cfg.read_text(encoding="utf-8"))


def test_a_model_write_preserves_keys_the_model_does_not_declare(tmp_path):
    """The blocker, pinned.

    `ADOSConfig` declares none of these four keys and drops them at load
    (`extra: "ignore"`). A write derived from the model must therefore never
    be a whole-document replace, or an operator toggling one unrelated setting
    silently disarms the FC-write arbiter and the hardware watchdog.
    """
    cfg = _write_doc(tmp_path)
    config = load_config(cfg)
    config.video.camera.hflip = False

    assert persist_config_model(config, path=cfg)

    after = _read(cfg)
    assert after["mavlink"]["injector_arbitration"] is True
    assert after["agent"]["headless"] is True
    assert after["video"]["wfb"]["reg_gate_strict"] is False
    assert after["network"]["watchdog"] == {"enabled": True, "timeout_s": 30}


def test_a_model_write_lands_only_the_leaves_the_caller_changed(tmp_path):
    """Repo rule 17: a node must keep tracking a shipped default it never set.

    `model_dump()` materialises every defaulted field, so the old write froze
    the whole model into the document and no future default change could reach
    that node again. The merge writes one leaf.
    """
    cfg = _write_doc(tmp_path)
    before = _read(cfg)
    config = load_config(cfg)
    config.video.camera.hflip = False

    result = persist_config_model(config, path=cfg)

    assert result.changed == ("video.camera.hflip",)
    after = _read(cfg)
    assert after["video"]["camera"]["hflip"] is False
    # Same key set, same sections: nothing was added and nothing removed.
    assert sorted(after) == sorted(before)
    assert sorted(after["video"]) == sorted(before["video"])
    assert sorted(after["video"]["camera"]) == sorted(before["video"]["camera"])
    assert "logging" not in after and "security" not in after


def test_a_model_write_that_changes_nothing_writes_nothing(tmp_path):
    """A caller that mutated no field must not rewrite the document.

    Reporting success is right — the file already says this — but touching the
    bytes is not: it would be an unnecessary rewrite of the document holding
    the radio pairing key on every no-op PUT.
    """
    cfg = _write_doc(tmp_path)
    before = cfg.read_bytes()

    result = persist_config_model(load_config(cfg), path=cfg)

    assert result.ok is True
    assert result.wrote is False
    assert cfg.read_bytes() == before


def test_a_nested_section_is_merged_not_replaced(tmp_path):
    """Writing one key inside a section must leave its siblings alone."""
    cfg = _write_doc(tmp_path)

    assert merge_into_config({"video": {"wfb": {"channel": 161}}}, path=cfg)

    wfb = _read(cfg)["video"]["wfb"]
    assert wfb["channel"] == 161
    assert wfb["reg_gate_strict"] is False
    # The sibling subsection under the same parent is untouched too.
    assert _read(cfg)["video"]["camera"]["width"] == 1920


def test_a_dotted_write_materialises_missing_levels(tmp_path):
    """A key whose parent section is absent still lands, without disturbing
    the sections that are present."""
    cfg = _write_doc(tmp_path)

    assert set_config_values({"mcp.token_accept_enabled": True}, path=cfg)

    after = _read(cfg)
    assert after["mcp"] == {"token_accept_enabled": True}
    assert after["mavlink"]["injector_arbitration"] is True


def test_a_free_form_dict_field_is_replaced_so_a_removal_lands(tmp_path):
    """`network.mac_pin.overrides` is a value, not a section.

    A `dict[str, str]` config field has to be written whole: merging it would
    resurrect the entry `DELETE /api/network/mac/<iface>` just removed, and the
    unpin would silently not unpin.
    """
    cfg = _write_doc(
        tmp_path,
        "network:\n"
        "  watchdog:\n"
        "    enabled: true\n"
        "  mac_pin:\n"
        "    overrides:\n"
        "      wlan0: aa:bb:cc:dd:ee:ff\n"
        "      wlan1: 11:22:33:44:55:66\n",
    )
    config = load_config(cfg)
    assert set(config.network.mac_pin.overrides) == {"wlan0", "wlan1"}

    config.network.mac_pin.overrides = {"wlan1": "11:22:33:44:55:66"}
    assert persist_config_model(config, path=cfg)

    after = _read(cfg)
    assert after["network"]["mac_pin"]["overrides"] == {"wlan1": "11:22:33:44:55:66"}
    assert after["network"]["watchdog"]["enabled"] is True


def test_a_failed_write_leaves_the_original_document_intact(tmp_path, monkeypatch):
    """The atomic-write contract: a write that cannot land is not a truncation.

    The temp-file-plus-rename means a reader sees either the whole old document
    or the whole new one. The failure must also be reported with a reason, not
    as a bare false — the GCS renders `persist_error`, and a silent false has
    it toast "Saved".
    """
    cfg = _write_doc(tmp_path)
    before = cfg.read_bytes()

    def _explode(*_args, **_kwargs):
        raise OSError("read-only filesystem")

    monkeypatch.setattr("ados.core.config.writer.atomic_write_text", _explode)

    result = merge_into_config({"video": {"wfb": {"channel": 161}}}, path=cfg)

    assert result.ok is False
    assert result.error and "read-only filesystem" in result.error
    assert cfg.read_bytes() == before
    # No temp file left behind to be mistaken for the document.
    assert [p.name for p in tmp_path.iterdir() if p.suffix == ".tmp"] == []


def test_a_write_declines_rather_than_losing_a_concurrent_update(tmp_path):
    """A held exclusive lock stands in for another writer mid read-modify-write.

    Proceeding without the lock does not corrupt the file — `os.replace` is
    atomic — it *loses* the other writer's update, silently, on the document
    that carries the radio pairing key, the profile and the role.
    """
    cfg = _write_doc(tmp_path)
    before = cfg.read_bytes()
    lock = tmp_path / "config.yaml.lock"

    holder = os.open(str(lock), os.O_CREAT | os.O_WRONLY, 0o600)
    try:
        fcntl.flock(holder, fcntl.LOCK_EX)
        result = merge_into_config(
            {"video": {"wfb": {"channel": 161}}}, path=cfg, timeout_s=0.2
        )
    finally:
        os.close(holder)

    assert result.ok is False
    assert result.locked is False
    assert result.error and "lock" in result.error
    assert cfg.read_bytes() == before


def test_an_unparseable_document_is_never_written_over(tmp_path):
    """Writing over a document we could not parse is a truncation of whatever
    the operator actually has on that node."""
    cfg = tmp_path / "config.yaml"
    cfg.write_text("agent: {name: [unclosed\n", encoding="utf-8")
    before = cfg.read_bytes()

    result = merge_into_config({"video": {"wfb": {"channel": 161}}}, path=cfg)

    assert result.ok is False
    assert result.error and "unparseable" in result.error
    assert cfg.read_bytes() == before


def test_a_write_creates_a_minimal_document_when_none_exists(tmp_path):
    """A fresh node's first write must record what differs from the shipped
    defaults, not a snapshot of every default."""
    cfg = tmp_path / "config.yaml"
    config = ADOSConfig()
    config.agent.name = "fresh-node"

    assert persist_config_model(config, path=cfg)

    after = _read(cfg)
    assert after["agent"]["name"] == "fresh-node"
    assert len(after) < len(ADOSConfig.model_fields)


def test_a_mutator_that_raises_is_a_failed_write_not_a_partial_one(tmp_path):
    """A caller bug must not leave a half-applied document on disk."""
    cfg = _write_doc(tmp_path)
    before = cfg.read_bytes()

    def _half_apply(document: dict) -> None:
        document["video"]["wfb"]["channel"] = 161
        raise RuntimeError("caller blew up")

    result = update_config(_half_apply, path=cfg)

    assert result.ok is False
    assert result.error and "caller blew up" in result.error
    assert cfg.read_bytes() == before


def test_the_write_result_is_falsy_only_on_failure():
    """Every historical callsite reads `bool(save_config())`; the richer return
    type must not change what those callsites conclude."""
    assert bool(ConfigWriteResult(ok=True))
    assert not bool(ConfigWriteResult(ok=False, error="nope"))


@pytest.mark.parametrize("mode", [0o600, 0o640])
def test_a_write_keeps_the_document_mode(tmp_path, mode):
    """The document carries the MQTT password, the API key, the HMAC secret and
    the AP passphrase. A write must not widen its mode."""
    cfg = _write_doc(tmp_path)
    os.chmod(cfg, mode)

    assert merge_into_config({"video": {"wfb": {"channel": 161}}}, path=cfg)

    assert (os.stat(cfg).st_mode & 0o777) == mode
