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
        # The bind's shared radio keys sit outside /etc/ados, and the swarm
        # bus and presence beacon derive the fleet keys from the drone key.
        assert "drone.key" in names, "the fleet's radio and swarm-bus key"
        assert "gs.key" in names, "the ground side of the radio keypair"

        dirs = {p.name for p in paths.FACTORY_RESET_DIRS}
        assert "secrets" in dirs, "tunnel token, setup token, server API key"
        assert "wfb" in dirs, "the radio keypair is the fleet's join gate"

    def test_profile_conf_is_deliberately_kept(self):
        # It holds what the hardware IS, not who owns it, and carries no
        # secret. Removing it strips the profile marker, and a later bare
        # upgrade then reprofiles the box — which has already cost a reflash.
        everything = {p.name for p in (*paths.FACTORY_RESET_FILES, *paths.FACTORY_RESET_DIRS)}
        assert "profile.conf" not in everything

    def test_identity_and_configuration_are_erased(self):
        # A factory reset means the unit comes back indistinguishable from a
        # freshly flashed one, so identity goes with the credentials. The unit
        # reappears in the GCS as a new device and must be added again; that is
        # intended, and is the safe default before handing on the hardware.
        #
        # The shell script already erased these while the API path preserved
        # them, so the two disagreed about what a factory reset meant.
        everything = {p.name for p in (*paths.FACTORY_RESET_FILES, *paths.FACTORY_RESET_DIRS)}
        assert "device-id" in everything
        assert "config.yaml" in everything
        assert "ados" in everything, "/var/log/ados carries the previous holder's history"

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
            # `rm -rf /var/log/ados/*` empties the directory while keeping it,
            # so the glob names the contents rather than the directory. Compare
            # on the directory the canonical list actually names.
            if target.endswith("/*"):
                target = target[: -len("/*")]
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
