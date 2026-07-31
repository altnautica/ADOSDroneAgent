"""Factory reset must destroy every standing credential.

Factory reset is what an operator runs before handing a unit to somebody else,
so the bar is that nothing the previous holder knows still opens the box.

Three implementations existed and each carried its own list, so they drifted:
between them the dashboard PIN, the MCP token and the setup/tunnel secrets were
cleared by none of them. These tests pin the canonical set and assert the shell
script agrees with it, because the divergence — not any single omission — is
what let a credential survive.
"""

from __future__ import annotations

import re
from pathlib import Path

from ados.core import paths

REPO_ROOT = Path(__file__).resolve().parents[1]
RESET_SCRIPT = REPO_ROOT / "scripts" / "factory-reset.sh"


class TestCanonicalSet:
    def test_every_credential_that_grants_access_is_in_the_reset_set(self):
        # Each of these opens the box on its own. A reset that leaves any one
        # of them behind hands the next owner a unit the previous one can
        # still reach.
        names = {p.name for p in paths.FACTORY_RESET_FILES}
        assert "pairing.json" in names, "the API key the data plane accepts"
        assert "dashboard-pin.json" in names, "mints dashboard sessions"
        assert "mcp-token.json" in names, "a scoped bearer the auth edge accepts"
        assert "ap-passphrase" in names, "the access point's WPA2 key"

        dirs = {p.name for p in paths.FACTORY_RESET_DIRS}
        assert "secrets" in dirs, "tunnel token, setup token, server API key"
        assert "wfb" in dirs, "the radio keypair is the fleet's join gate"

    def test_profile_conf_is_deliberately_kept(self):
        # It holds what the hardware IS, not who owns it, and carries no
        # secret. Removing it strips the profile marker, and a later bare
        # upgrade then reprofiles the box — which has already cost a reflash.
        everything = {p.name for p in (*paths.FACTORY_RESET_FILES, *paths.FACTORY_RESET_DIRS)}
        assert "profile.conf" not in everything
        assert "device-id" not in everything

    def test_the_set_has_no_duplicates(self):
        entries = [*paths.FACTORY_RESET_FILES, *paths.FACTORY_RESET_DIRS]
        assert len(entries) == len(set(entries))


class TestShellScriptAgrees:
    """The shell script is the path that runs when the agent is too broken to
    serve its own API — i.e. exactly when a reset matters most. It must clear
    the same set, and nothing may be in one list and not the other."""

    def _removed_paths(self) -> set[str]:
        body = RESET_SCRIPT.read_text(encoding="utf-8")
        found = set()
        for m in re.finditer(r'rm\s+-[rf]*\s+"?([^"\s]+)"?', body):
            target = m.group(1)
            target = target.replace("$CONFIG_DIR", "/etc/ados").rstrip("/")
            found.add(Path(target).name)
        return found

    def test_the_script_clears_every_canonical_credential(self):
        removed = self._removed_paths()
        expected = {p.name for p in (*paths.FACTORY_RESET_FILES, *paths.FACTORY_RESET_DIRS)}
        missing = expected - removed
        assert not missing, (
            f"the shell reset does not clear {sorted(missing)}; it and "
            "paths.FACTORY_RESET_* have drifted apart again"
        )

    def test_the_script_does_not_remove_the_profile_marker(self):
        assert "profile.conf" not in self._removed_paths()


class TestPairManagerUsesTheSharedSet:
    def test_factory_reset_reads_the_canonical_lists(self):
        # A local copy of the list is how this drifted the first time.
        src = (REPO_ROOT / "src/ados/services/ground_station/pair_manager.py").read_text(
            encoding="utf-8"
        )
        assert "FACTORY_RESET_FILES" in src
        assert "FACTORY_RESET_DIRS" in src

    def test_it_clears_the_configured_hotspot_password(self):
        # Deleting /etc/ados/ap-passphrase alone did NOT re-key the AP:
        # ensure_passphrase prefers a configured network.hotspot.password over
        # generating one, so a configured rig came back on the same key while
        # the docstring claimed a fresh one.
        src = (REPO_ROOT / "src/ados/services/ground_station/pair_manager.py").read_text(
            encoding="utf-8"
        )
        assert "_clear_configured_hotspot_password" in src


class TestClearingTheHotspotPassword:
    def test_a_configured_password_is_removed_and_the_rest_survives(self, tmp_path, monkeypatch):
        import yaml

        from ados.services.ground_station import pair_manager as pm

        cfg = tmp_path / "config.yaml"
        cfg.write_text(
            yaml.safe_dump(
                {
                    "network": {"hotspot": {"password": "configured-value", "ssid": "ados-gs"}},
                    "video": {"wfb": {"channel": 149}},
                }
            ),
            encoding="utf-8",
        )
        monkeypatch.setattr(pm, "_CONFIG_PATH", cfg)
        # The writer refuses to run as non-root, which is correct on a rig and
        # inconvenient here. Stand in for the euid check rather than relaxing
        # it, so the guard keeps protecting the real path.
        monkeypatch.setattr(pm.os, "geteuid", lambda: 0)
        monkeypatch.setattr(pm, "_CONFIG_LOCK_PATH", tmp_path / "config.lock")

        pm._clear_configured_hotspot_password()

        after = yaml.safe_load(cfg.read_text(encoding="utf-8"))
        assert "password" not in after["network"]["hotspot"]
        # Everything else must survive: this is a targeted removal, not a wipe.
        assert after["network"]["hotspot"]["ssid"] == "ados-gs"
        assert after["video"]["wfb"]["channel"] == 149

    def test_no_configured_password_is_a_no_op(self, tmp_path, monkeypatch):
        import yaml

        from ados.services.ground_station import pair_manager as pm

        cfg = tmp_path / "config.yaml"
        original = {"network": {"hotspot": {"ssid": "ados-gs"}}}
        cfg.write_text(yaml.safe_dump(original), encoding="utf-8")
        monkeypatch.setattr(pm, "_CONFIG_PATH", cfg)

        pm._clear_configured_hotspot_password()

        assert yaml.safe_load(cfg.read_text(encoding="utf-8")) == original


class TestTheResetIsIsolatable:
    """A reset walks absolute paths. If a test cannot redirect them, a suite run
    as root wipes the machine it runs on — so the injection point is part of the
    contract, not a convenience."""

    def test_the_module_exposes_redirectable_reset_lists(self):
        from ados.services.ground_station import pair_manager as pm

        assert hasattr(pm, "_FACTORY_RESET_FILES")
        assert hasattr(pm, "_FACTORY_RESET_DIRS")

    def test_the_module_lists_are_the_canonical_ones_not_a_second_copy(self):
        # Aliases, not a re-listing: a private copy is how the three reset
        # implementations drifted apart in the first place.
        from ados.core import paths
        from ados.services.ground_station import pair_manager as pm

        assert pm._FACTORY_RESET_FILES == paths.FACTORY_RESET_FILES
        assert pm._FACTORY_RESET_DIRS == paths.FACTORY_RESET_DIRS

    def test_reset_touches_only_the_redirected_paths(self, tmp_path, monkeypatch):
        import asyncio

        from ados.services.ground_station import pair_manager as pm

        doomed = tmp_path / "doomed.json"
        doomed.write_text("secret", encoding="utf-8")
        doomed_dir = tmp_path / "doomed-dir"
        doomed_dir.mkdir()
        (doomed_dir / "inner").write_text("secret", encoding="utf-8")
        survivor = tmp_path / "survivor.txt"
        survivor.write_text("keep me", encoding="utf-8")

        monkeypatch.setattr(pm, "_FACTORY_RESET_FILES", (doomed,))
        monkeypatch.setattr(pm, "_FACTORY_RESET_DIRS", (doomed_dir,))
        monkeypatch.setattr(pm, "_clear_configured_hotspot_password", lambda: None)

        mgr = pm.PairManager(key_dir=str(tmp_path))
        asyncio.run(mgr.factory_reset("gs"))

        assert not doomed.exists()
        assert not doomed_dir.exists()
        assert survivor.read_text(encoding="utf-8") == "keep me"
