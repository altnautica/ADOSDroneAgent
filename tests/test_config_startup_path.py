"""The config read path is read-only, and migration is serialised by a lock.

Eleven units start concurrently on a node. Every one of them calls
``load_config()`` on its own startup path. When that call could also
*rewrite* ``/etc/ados/config.yaml`` — as it did, via four side-file
migrators that each serialised the whole merged mapping back over the file
with no lock — the startup path became eleven unsynchronised
read-modify-write cycles against the one file that carries the radio
pairing key, the profile and the role. The native writers do take
``/run/ados/config.yaml.lock``, which bought them nothing, because the
process racing them was not taking it.

So the rule these tests pin is: reading config never writes, and the
migration that does write happens once, off the startup path, under the
same lock the native writers use, re-reading the file inside the lock so a
concurrent write is never lost.
"""

from __future__ import annotations

import fcntl
import os
from pathlib import Path

import yaml

from ados.core.config import load_config
from ados.core.config.maintenance import migrate_config_file

# A config that trips every migrator at once: a legacy `scripting` block to
# relocate, and a recorded ws_proxy_enforce_auth=false to drop.
_LEGACY_CONFIG = {
    "agent": {"name": "test-node", "profile": "drone"},
    "mavlink": {"system_id": 42, "ws_proxy_enforce_auth": False},
    "scripting": {
        "rest_api": {"enabled": True, "host": "127.0.0.1", "port": 8099},
        "mission_control_url": "https://gcs.example.com",
    },
    "video": {"wfb": {"paired_at": "2026-01-01T00:00:00Z"}},
}

_LEGACY_GS_UI = {
    "share_uplink": True,
    "oled": {"enabled": True},
    "buttons": {"count": 3},
    "screens": {"default": "status"},
}


def _write_config(tmp_path: Path) -> Path:
    cfg = tmp_path / "config.yaml"
    # Written as raw text, not via `safe_dump`, because `safe_dump` quotes a
    # str that would otherwise resolve as a timestamp — and quoting is
    # exactly what the native writers do NOT do. An unquoted ISO-8601 value
    # is what is actually on a paired node's disk, and it is the input that
    # makes the loader choice observable.
    cfg.write_text(
        yaml.safe_dump(
            {k: v for k, v in _LEGACY_CONFIG.items() if k != "video"},
            sort_keys=False,
        )
        + "video:\n  wfb:\n    paired_at: 2026-01-01T00:00:00Z\n"
    )
    return cfg


def _point_legacy_gs_ui(monkeypatch, tmp_path: Path) -> Path:
    legacy = tmp_path / "ground-station-ui.json"
    import json

    legacy.write_text(json.dumps(_LEGACY_GS_UI))
    monkeypatch.setattr(
        "ados.core.config._migrators._LEGACY_GS_UI_PATH", legacy
    )
    return legacy


def _point_lock(monkeypatch, tmp_path: Path) -> Path:
    lock = tmp_path / "config.yaml.lock"
    monkeypatch.setattr("ados.core.config._lock.CONFIG_LOCK", lock)
    return lock


def _ledger(tmp_path: Path) -> Path:
    return tmp_path / "config-migrations.json"


# ---------------------------------------------------------------------------
# The read path never writes
# ---------------------------------------------------------------------------


def test_load_config_does_not_write_the_config_file(monkeypatch, tmp_path):
    """The whole defect in one assertion: loading config left the file alone.

    Every migrator used to flush its result back to disk from inside
    ``load_config()``. On a node where all four fire, eleven units starting
    at once each performed a full-file rewrite of the file the native
    daemons were writing under a lock.
    """
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)

    before = cfg.read_bytes()
    before_mtime = cfg.stat().st_mtime_ns

    load_config(cfg)

    assert cfg.read_bytes() == before, (
        "load_config() rewrote the config file: the startup path is "
        "migrating again"
    )
    assert cfg.stat().st_mtime_ns == before_mtime


def test_load_config_does_not_write_even_with_no_lock_available(
    monkeypatch, tmp_path
):
    """A read is still a read when the lock directory does not exist.

    The lock lives on tmpfs under ``/run``. A read that cannot take it must
    still complete — eleven units must not fail to start because ``/run``
    is not writable — and must still not write.
    """
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    monkeypatch.setattr(
        "ados.core.config._lock.CONFIG_LOCK",
        tmp_path / "absent" / "nested" / "config.yaml.lock",
    )

    before = cfg.read_bytes()
    conf = load_config(cfg)

    assert cfg.read_bytes() == before
    assert conf.agent.name == "test-node"


def test_load_config_still_applies_every_migration_in_memory(
    monkeypatch, tmp_path
):
    """Dropping the disk flush must not drop the normalisation.

    A node that has not yet had the maintenance pass run still has to
    behave as though it had for every idempotent shape change: the
    relocated api block and the legacy ground-station values all have to
    reach the running process from the read path alone.
    """
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)

    conf = load_config(cfg)

    # scripting.rest_api -> api.rest
    assert conf.api.rest.port == 8099
    assert conf.api.mission_control_url == "https://gcs.example.com"
    # legacy ground-station-ui.json -> ground_station.*
    assert conf.ground_station.share_uplink is True
    assert conf.ground_station.ui.oled["enabled"] is True
    # A recorded `false` is a one-shot cleanup, not a normalisation, so the
    # read path leaves it exactly as the operator's file has it. See
    # test_a_recorded_enforce_false_survives_the_read_path.
    assert conf.mavlink.ws_proxy_enforce_auth is False
    # and the operator's own value is untouched
    assert conf.mavlink.system_id == 42


def test_load_config_reads_the_already_migrated_shape_without_the_side_file(
    monkeypatch, tmp_path
):
    """Once migrated, the legacy side file is never read again.

    The migrators preserve ``ground-station-ui.json`` on disk for rollback,
    so it is still there on every migrated ground station. Checking the
    destination before the source keeps that from being a file read on
    every one of the seventeen ``load_config()`` call sites, forever.
    """
    cfg = tmp_path / "config.yaml"
    cfg.write_text(
        yaml.safe_dump(
            {
                "ground_station": {
                    "share_uplink": False,
                    "ui": {
                        "oled": {"enabled": False},
                        "buttons": {},
                        "screens": {},
                    },
                }
            }
        )
    )
    legacy = _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)

    reads: list[str] = []
    real_read_text = Path.read_text

    def _tracking_read_text(self, *args, **kwargs):
        reads.append(str(self))
        return real_read_text(self, *args, **kwargs)

    monkeypatch.setattr(Path, "read_text", _tracking_read_text)

    conf = load_config(cfg)

    assert conf.ground_station.share_uplink is False
    assert str(legacy) not in reads


# ---------------------------------------------------------------------------
# The maintenance pass writes, under the lock, with a re-read inside it
# ---------------------------------------------------------------------------


def test_migrate_config_file_persists_every_migration(monkeypatch, tmp_path):
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)

    result = migrate_config_file(cfg, ledger_path=_ledger(tmp_path))

    assert result.locked is True
    assert result.changed is True
    assert set(result.applied) == {
        "share_uplink_from_legacy_json",
        "gs_ui_from_legacy_json",
        "api_from_scripting",
        "ws_proxy_enforce_default",
    }

    on_disk = yaml.safe_load(cfg.read_text())
    assert on_disk["api"]["rest"]["port"] == 8099
    assert on_disk["ground_station"]["share_uplink"] is True
    assert "ws_proxy_enforce_auth" not in on_disk["mavlink"]
    # Everything an operator tuned beside the migrated keys comes back.
    assert on_disk["mavlink"]["system_id"] == 42
    # And a timestamp comes back byte-identical. A read-modify-write that
    # used the stock loader would resolve this to a datetime and dump a
    # different string, silently rewriting a pairing timestamp on an
    # unrelated migration.
    assert on_disk["video"]["wfb"]["paired_at"] == "2026-01-01T00:00:00Z"


def test_migrate_config_file_is_idempotent(monkeypatch, tmp_path):
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)
    ledger = _ledger(tmp_path)

    migrate_config_file(cfg, ledger_path=ledger)
    after_first = cfg.read_bytes()

    second = migrate_config_file(cfg, ledger_path=ledger)

    assert second.changed is False
    assert second.applied == []
    assert cfg.read_bytes() == after_first


# ---------------------------------------------------------------------------
# A one-shot cleanup runs once. This is the distinction a pre-existing test
# in tests/test_config.py caught: `ws_proxy_enforce_auth: false` written by
# an old build is residue to remove, but the same value written by an
# operator afterwards is a deliberate opt-out that must survive.
# ---------------------------------------------------------------------------


def test_a_recorded_enforce_false_survives_the_read_path(monkeypatch, tmp_path):
    """The read path never applies a one-shot cleanup.

    Applying it on every read would mean an operator could not turn WS
    authentication enforcement off at all — the value would be stripped
    from memory on every single load, forever, with the config file still
    showing it set.
    """
    cfg = tmp_path / "config.yaml"
    cfg.write_text(yaml.safe_dump({"mavlink": {"ws_proxy_enforce_auth": False}}))
    _point_lock(monkeypatch, tmp_path)

    assert load_config(cfg).mavlink.ws_proxy_enforce_auth is False


def test_a_deliberate_enforce_false_survives_a_later_migration_pass(
    monkeypatch, tmp_path
):
    """Once the cleanup is recorded as done, it never fires again.

    An operator with a third-party client that cannot present a credential
    sets this after the upgrade. A cleanup with no ledger would remove it
    on the next pass and every pass after that.
    """
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)
    ledger = _ledger(tmp_path)

    # The upgrade pass strips the residue from the old build.
    assert "ws_proxy_enforce_default" in migrate_config_file(
        cfg, ledger_path=ledger
    ).applied

    # The operator then opts out, deliberately.
    raw = yaml.safe_load(cfg.read_text())
    raw["mavlink"]["ws_proxy_enforce_auth"] = False
    cfg.write_text(yaml.safe_dump(raw, sort_keys=False))

    result = migrate_config_file(cfg, ledger_path=ledger)

    assert "ws_proxy_enforce_default" not in result.applied
    on_disk = yaml.safe_load(cfg.read_text())
    assert on_disk["mavlink"]["ws_proxy_enforce_auth"] is False
    assert load_config(cfg).mavlink.ws_proxy_enforce_auth is False


def test_a_failed_config_write_does_not_record_the_one_shot(
    monkeypatch, tmp_path
):
    """Recording a cleanup whose write failed would skip it on the retry
    that exists to fix the failure."""
    from ados.core.config import maintenance

    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)
    ledger = _ledger(tmp_path)

    def _explode(*args, **kwargs):
        raise OSError("read-only filesystem")

    monkeypatch.setattr(maintenance, "_atomic_write", _explode)
    result = migrate_config_file(cfg, ledger_path=ledger)

    assert result.changed is False
    assert result.error is not None
    assert maintenance.completed_one_shots(ledger) == set()


def test_a_garbled_ledger_reads_as_nothing_done(tmp_path):
    """Erring toward re-running a no-op cleanup, never toward skipping one a
    field node still needs."""
    from ados.core.config import maintenance

    ledger = _ledger(tmp_path)
    ledger.write_text("{ not json")
    assert maintenance.completed_one_shots(ledger) == set()


def test_migrate_config_file_refuses_to_write_without_the_lock(
    monkeypatch, tmp_path
):
    """This is the concurrent hazard, made deterministic.

    A held exclusive lock stands in for the native writer that is mid
    read-modify-write. The migration must decline rather than proceed:
    proceeding is precisely how the writer's update gets lost, because both
    sides serialise a whole mapping they read before the other one wrote.
    """
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    lock = _point_lock(monkeypatch, tmp_path)

    before = cfg.read_bytes()

    holder = os.open(str(lock), os.O_CREAT | os.O_WRONLY, 0o600)
    try:
        fcntl.flock(holder, fcntl.LOCK_EX)
        result = migrate_config_file(cfg, timeout_s=0.2)
    finally:
        os.close(holder)

    assert result.locked is False
    assert result.changed is False
    assert cfg.read_bytes() == before, (
        "the migration wrote while another writer held the lock: a "
        "concurrent update is being lost"
    )


def test_migrate_config_file_rereads_inside_the_lock(monkeypatch, tmp_path):
    """The read that feeds the write happens under the lock, not before it.

    A migration that decided its output from a copy read before acquiring
    the lock would serialise a mapping that predates whatever the lock
    holder just wrote, silently reverting it. So the pass is handed a fresh
    read taken after acquisition — modelled here by a writer that lands its
    change in the window between the caller's own read and the lock being
    taken.
    """
    cfg = _write_config(tmp_path)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)

    import ados.core.config._lock as lock_mod

    real_exclusive = lock_mod.exclusive_config_lock
    concurrent = {
        **_LEGACY_CONFIG,
        "video": {"wfb": {"paired_at": "2026-01-01T00:00:00Z", "key": "abc123"}},
    }

    def _writing_lock(*args, **kwargs):
        # A native writer lands a pairing key while we are acquiring.
        cfg.write_text(yaml.safe_dump(concurrent, sort_keys=False))
        return real_exclusive(*args, **kwargs)

    monkeypatch.setattr(lock_mod, "exclusive_config_lock", _writing_lock)
    monkeypatch.setattr(
        "ados.core.config.maintenance.exclusive_config_lock", _writing_lock
    )

    result = migrate_config_file(cfg)

    assert result.changed is True
    on_disk = yaml.safe_load(cfg.read_text())
    assert on_disk["video"]["wfb"]["key"] == "abc123", (
        "the migration reverted a write that landed before it took the "
        "lock: it is not re-reading inside the lock"
    )
    assert on_disk["api"]["rest"]["port"] == 8099


def test_migrate_config_file_refuses_an_unparseable_config(
    monkeypatch, tmp_path
):
    """Same posture as the native writers: never write over a file we
    could not parse, because the write would be a truncation."""
    cfg = tmp_path / "config.yaml"
    cfg.write_text("agent:\n  profile: drone\nvideo: [unclosed\n")
    _point_lock(monkeypatch, tmp_path)

    before = cfg.read_bytes()
    result = migrate_config_file(cfg)

    assert result.changed is False
    assert result.error is not None
    assert cfg.read_bytes() == before


def test_migrate_config_file_preserves_the_file_mode(monkeypatch, tmp_path):
    """The config carries secrets and ships 0o600. An atomic replace that
    dropped the mode would publish them to every local account."""
    cfg = _write_config(tmp_path)
    os.chmod(cfg, 0o600)
    _point_legacy_gs_ui(monkeypatch, tmp_path)
    _point_lock(monkeypatch, tmp_path)

    assert migrate_config_file(cfg).changed is True
    assert (os.stat(cfg).st_mode & 0o777) == 0o600


def test_migrate_config_file_on_an_absent_config_is_a_clean_noop(
    monkeypatch, tmp_path
):
    _point_lock(monkeypatch, tmp_path)
    result = migrate_config_file(tmp_path / "nope.yaml")
    assert result.changed is False
    assert result.error is None
    assert not (tmp_path / "nope.yaml").exists()
