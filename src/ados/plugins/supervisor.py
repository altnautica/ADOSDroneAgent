"""PluginSupervisor: lifecycle and state for installed plugins.

Sub-supervisor under the existing :mod:`ados.core.supervisor`.
Responsibilities:

* Discover built-in plugins via the ``ados.plugins`` entry-points group.
* Read on-disk install state (``/var/ados/state/plugin-state.json``)
  and reconcile against unpacked third-party archives at
  ``/var/ados/plugins/<id>/``.
* Install a ``.adosplug`` archive: verify signature, run manifest
  compatibility checks, unpack, write systemd unit, persist state.
* Enable a plugin: ``systemctl enable + start`` (or import + lifecycle
  hook for inprocess built-ins).
* Disable a plugin: ``systemctl stop + disable``.
* Remove a plugin: stop, remove unit, delete unpacked dir, delete state.
* Read-only queries used by the CLI and REST API.

The supervisor does not run plugin code itself for subprocess plugins.
The actual plugin runner is the ``ados-plugin-runner`` binary that
systemd starts; see :mod:`ados.plugins.runner`.

Compatibility checks at install time:

* Plugin ``compatibility.ados_version`` must include the running
  agent's version.
* Plugin ``compatibility.supported_boards`` (if non-empty) must include
  the current HAL board id.
* ``isolation: inprocess`` requires first-party signer (see
  :func:`ados.plugins.signing.is_first_party_signer`).
* ``isolation: inline`` (GCS) requires first-party signer; enforced on
  the GCS side too.
"""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import (
    PLUGIN_LOG_DIR,
    PLUGINS_INSTALL_DIR,
)
from ados.plugins import host_control
from ados.plugins import systemd as _systemd
from ados.plugins.archive import (
    MANIFEST_FILENAME,
    open_archive,
    unpack_to,
    verify_entrypoints_present,
)
from ados.plugins.errors import (
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.loader import load_builtin_manifests
from ados.plugins.manifest import PluginManifest
from ados.plugins.ready_check import PROBE_TIMEOUT_S, HttpProbe, parse_ready_check
from ados.plugins.signing import (
    is_first_party_signer,
    load_trusted_keys,
    verify_archive_signature,
)
from ados.services.signing import verify_signature

# A high-risk plugin may be granted ahead of time so it installs unattended
# with a SIGNED grant rather than a human clicking approve. A grant lives at
# /etc/ados/plugin-grants/<plugin_id>.json.
GRANT_DIR = "/etc/ados/plugin-grants"
from ados.plugins.state import (
    PluginInstall,
    filter_permissions_against_manifest,
    find_install,
    grant_permission,
    load_state,
    remove_install,
    revoke_permission,
    save_state,
    state_lock,
    upsert_install,
)
from ados.plugins.systemd import (
    probe_command,
    render_service_unit,
    render_unit,
    service_unit_name_for,
    service_unit_path_for,
    slice_unit_content,
    unit_name_for,
    unit_path_for,
)

log = get_logger("plugins.supervisor")

import ados as _ados


@dataclass
class InstallResult:
    plugin_id: str
    version: str
    signer_id: str | None
    risk: str
    permissions_requested: list[str]


class PluginSupervisor:
    """Plugin lifecycle controller.

    Constructed once per agent. The supervisor is sync; lifecycle calls
    that touch ``systemctl`` are blocking on a few hundred milliseconds,
    consistent with the rest of the agent service control plane.
    """

    def __init__(
        self,
        *,
        install_dir: Path | None = None,
        require_signed: bool = True,
        current_board_id: str | None = None,
        current_board_tier: int | None = None,
    ) -> None:
        self._install_dir = install_dir or PLUGINS_INSTALL_DIR
        self._require_signed = require_signed
        self._current_board_id = current_board_id
        self._current_board_tier = current_board_tier
        self._installs: list[PluginInstall] = []
        self._builtin: dict[str, PluginManifest] = {}

    # ------------------------------------------------------------------
    # Boot-time discovery
    # ------------------------------------------------------------------

    def discover(self) -> None:
        """Read on-disk state, load built-in entry-points, sanity-check.

        Also filters in-memory permission grants down to what the
        manifest currently declares, defending against a tampered
        state file.
        """
        self._installs = load_state()
        for manifest in load_builtin_manifests():
            self._builtin[manifest.id] = manifest
        for install in self._installs:
            try:
                manifest = self._manifest_for(install.plugin_id)
            except SupervisorError:
                continue
            filter_permissions_against_manifest(
                install, manifest.declared_permissions()
            )
        log.info(
            "plugin_supervisor_discovered",
            builtin_count=len(self._builtin),
            installed_count=len(self._installs),
        )

    def builtin_manifests(self) -> dict[str, PluginManifest]:
        return dict(self._builtin)

    def installs(self) -> list[PluginInstall]:
        return list(self._installs)

    def find_install(self, plugin_id: str) -> PluginInstall | None:
        """Return the install record for ``plugin_id`` or None if not installed."""
        return find_install(self._installs, plugin_id)

    # ------------------------------------------------------------------
    # Install / enable / disable / remove
    # ------------------------------------------------------------------

    def install_archive(
        self, archive_path: Path, *, allow_downgrade: bool = False
    ) -> InstallResult:
        """Install a ``.adosplug`` archive. Returns a summary.

        Caller is responsible for prompting the operator to approve
        permissions BEFORE calling this method. The supervisor records
        every requested permission as ``granted=False`` initially; the
        operator-side flow then calls :meth:`grant_permission` per
        approved permission.

        ``allow_downgrade`` permits a version lower than the one already
        installed. Off by default: a signed older archive silently replacing
        a newer install is a rollback attack on any plugin whose signing key
        is held — the signature verifies, the operator sees success, and the
        plugin is back to a version with a known hole. A deliberate rollback
        (a bad release) is a real need, so it is explicit rather than
        implicit.

        Wrapped in :func:`state_lock` so concurrent install/remove
        flows on the same host serialize.
        """
        contents = open_archive(archive_path)
        manifest = contents.manifest

        if self._require_signed:
            if contents.signer_id is None or contents.signature_b64 is None:
                raise SignatureError(
                    SignatureError.KIND_MISSING,
                    f"plugin {manifest.id}: archive is unsigned",
                )
            verify_archive_signature(
                contents.payload_hash,
                contents.signature_b64,
                contents.signer_id,
            )

        self._check_compatibility(manifest, contents.signer_id)
        self._reject_inline_for_third_party(manifest, contents.signer_id)
        if not allow_downgrade:
            self._reject_downgrade(manifest)

        with state_lock():
            target = self._install_dir / manifest.id
            if target.exists():
                shutil.rmtree(target)
            unpack_to(contents.raw_archive_bytes, target)

            # A manifest that declares a GCS half (or a rust agent binary)
            # must actually ship the file it points at. Fail the install
            # loudly here rather than letting a missing bundle surface
            # later as an empty iframe or a unit that cannot start.
            unpacked = {
                p.relative_to(target).as_posix()
                for p in target.rglob("*")
                if p.is_file()
            }
            verify_entrypoints_present(manifest, unpacked)

            # Install the agent half's vendored wheels into a per-plugin site
            # dir. The archive layout has always documented an
            # ``agent/wheel/`` slot and nothing installed it, so any agent
            # half importing a package outside the agent venv died at
            # entrypoint import with a unit flapping on an ImportError.
            self._install_agent_wheels(manifest, target)

            # Write systemd unit for subprocess agent halves. A fresh install
            # has granted nothing yet, so the unit renders with the most
            # restrictive sandbox; grant/revoke re-renders it.
            if (
                manifest.agent is not None
                and manifest.agent.isolation == "subprocess"
            ):
                self._ensure_slice_exists()
                unit_path = unit_path_for(manifest.id)
                unit_path.write_text(
                    render_unit(manifest, self._install_dir, ()),
                    encoding="utf-8",
                )
                self._systemctl("daemon-reload")

            manifest_hash = hashlib.sha256(
                (target / MANIFEST_FILENAME).read_bytes()
            ).hexdigest()

            install = PluginInstall(
                plugin_id=manifest.id,
                version=manifest.version,
                source="local_file",
                source_uri=str(archive_path),
                signer_id=contents.signer_id,
                manifest_hash=manifest_hash,
                status="installed",
                installed_at=_now_ms(),
                permissions={},
            )
            self._installs = upsert_install(self._installs, install)
            save_state(self._installs)

        log.info(
            "plugin_installed",
            plugin_id=manifest.id,
            version=manifest.version,
            signer_id=contents.signer_id,
        )
        return InstallResult(
            plugin_id=manifest.id,
            version=manifest.version,
            signer_id=contents.signer_id,
            risk=manifest.risk,
            permissions_requested=sorted(manifest.declared_permissions()),
        )

    def grant_covers(self, plugin_id: str, requested: list[str]) -> list[str]:
        """The subset of `requested` covered by a valid signed grant.

        A grant at ``/etc/ados/plugin-grants/<plugin_id>.json`` is
        ``{plugin_id, permissions: [cap id...], signer, sig}`` where ``sig``
        is base64 Ed25519 over the canonical JSON (``{plugin_id,
        permissions(sorted), signer}``, ``sort_keys``, compact separators),
        verifiable under a key in ``/etc/ados/plugin-keys/``. An absent,
        malformed, unknown-signer, or bad-signature grant yields an empty
        list (fail closed). The grant can only vouch for permissions the
        archive actually declared — a grant naming an undeclared capability
        is meaningless and ignored.
        """
        if not requested:
            return []
        grant_path = Path(GRANT_DIR) / f"{plugin_id}.json"
        if not grant_path.is_file():
            return []
        try:
            grant = json.loads(grant_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return []
        if not isinstance(grant, dict):
            return []
        signer = grant.get("signer")
        sig = grant.get("sig")
        granted_perms = grant.get("permissions")
        if not isinstance(signer, str) or not isinstance(sig, str):
            return []
        if not isinstance(granted_perms, list) or not all(
            isinstance(p, str) for p in granted_perms
        ):
            return []
        keys = load_trusted_keys()
        key = keys.get(signer)
        if key is None:
            return []
        canonical = json.dumps(
            {
                "plugin_id": plugin_id,
                "permissions": sorted(granted_perms),
                "signer": signer,
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        if not verify_signature(canonical, sig, key.pem):
            return []
        covered = set(granted_perms)
        return [p for p in sorted(set(requested)) if p in covered]

    def grant_permission(self, plugin_id: str, permission_id: str) -> None:
        with state_lock():
            install = self._require_install(plugin_id)
            manifest = self._manifest_for(plugin_id)
            if permission_id not in manifest.declared_permissions():
                raise SupervisorError(
                    f"plugin {plugin_id} did not declare permission {permission_id}"
                )
            grant_permission(install, permission_id)
            save_state(self._installs)
        self._apply_permission_change(plugin_id, manifest)

    def revoke_permission(self, plugin_id: str, permission_id: str) -> None:
        """Revoke a granted permission. Takes effect immediately.

        See :meth:`_apply_permission_change` for what "immediately" costs
        and why it is not optional.
        """
        with state_lock():
            install = self._require_install(plugin_id)
            manifest = self._manifest_for(plugin_id)
            revoke_permission(install, permission_id)
            save_state(self._installs)
        self._apply_permission_change(plugin_id, manifest)

    def _apply_permission_change(
        self, plugin_id: str, manifest: PluginManifest
    ) -> None:
        """Make a permission change real, right now.

        Two mechanisms enforce a plugin's capabilities and both are snapshots
        taken when something was last written, so a state write alone changes
        nothing a running plugin can observe:

        1. **The sandbox.** ``hardware.*``, ``network.outbound`` and
           ``filesystem.host`` are enforced by the generated unit's cgroup
           device policy, address-family filter and mount namespace (see
           :func:`ados.plugins.systemd.sandbox_directives`). systemd applies
           those at exec time, so the unit is re-rendered and the plugin
           restarted — but only when the grant actually changed the unit text,
           because bouncing a running plugin to apply a token change it can
           receive live would be a needless gap in coverage.
        2. **The capability token.** Every wire-gated capability rides in the
           plugin's HMAC token. The live plugin host re-mints it from the new
           grant set and pushes it into the open connection, so the plugin's
           next request is gated against the new set with no restart and no
           dropped session.

        Before this existed a revoke was a security control that reported
        itself applied: the CLI and the GCS said success while the plugin kept
        its old grant set until someone restarted ``ados-plugin-host``.
        """
        install = find_install(self._installs, plugin_id)
        granted = (
            sorted(
                pid for pid, g in install.permissions.items() if g.granted
            )
            if install is not None
            else []
        )

        if manifest.agent is not None and manifest.agent.isolation == "subprocess":
            unit_path = unit_path_for(plugin_id)
            rendered = render_unit(manifest, self._install_dir, granted)
            previous = (
                unit_path.read_text(encoding="utf-8")
                if unit_path.exists()
                else ""
            )
            if previous != rendered:
                unit_path.write_text(rendered, encoding="utf-8")
                self._systemctl("daemon-reload")
                if install is not None and install.status == "running":
                    self._systemctl("restart", unit_name_for(plugin_id))

        # Re-mint the live token. A plugin host that is not up has nothing to
        # re-mint against and picks the new grant set off state on its next
        # poll, so an unreachable control socket is logged, not an error.
        if not host_control.rotate_token(plugin_id):
            log.info(
                "plugin_permission_change_pending_host_poll",
                plugin_id=plugin_id,
                detail="plugin host control socket unreachable; the new grant "
                "set applies on its next state poll",
            )

    def enable(self, plugin_id: str) -> None:
        with state_lock():
            install = self._require_install(plugin_id)
            manifest = self._manifest_for(plugin_id)
            if install.status == "running":
                return  # idempotent; state already correct
            if manifest.agent is None or manifest.agent.isolation == "inprocess":
                install.status = "enabled"
                install.enabled_at = _now_ms()
                save_state(self._installs)
                return
            # Flip state to enabled and persist BEFORE starting the unit, then
            # have the plugin host bind the socket and write the token env off
            # that state. The order is the whole fix for the silently-inert
            # plugin: the runner reads ADOS_PLUGIN_SOCKET / ADOS_PLUGIN_TOKEN
            # from its environment at start, so both have to exist first. The
            # runner retries rather than degrading if they do not, but making
            # it wait for a poll interval on every enable would be a gratuitous
            # few seconds of dead plugin.
            install.status = "enabled"
            install.enabled_at = _now_ms()
            save_state(self._installs)
            host_control.reconcile()
            unit = unit_name_for(plugin_id)
            self._systemctl("enable", unit)
            self._systemctl("start", unit)
            # Start each declared extra service under its own unit. These
            # are ADDITIONAL to the main runner unit above. A failure to
            # render or start one is surfaced as not-ready(reason) rather
            # than crashing the enable — the plugin's main half still runs.
            self._start_declared_services(plugin_id, manifest)
            install.status = "running"
            # Probe + persist readiness of the declared services so the
            # heartbeat surfaces it without a separate poll.
            install.service_status = self._compute_service_readiness(
                plugin_id, manifest
            )
            save_state(self._installs)
        log.info("plugin_enabled", plugin_id=plugin_id)

    def disable(self, plugin_id: str) -> None:
        with state_lock():
            install = self._require_install(plugin_id)
            manifest = self._manifest_for(plugin_id)
            if install.status == "disabled":
                return  # idempotent; state already correct
            if (
                manifest.agent is not None
                and manifest.agent.isolation == "subprocess"
            ):
                # Stop the declared extra services first, then the main
                # runner unit. Best-effort on the extras so a missing
                # unit does not block the main teardown.
                self._stop_declared_services(plugin_id, manifest)
                unit = unit_name_for(plugin_id)
                self._systemctl("stop", unit)
                self._systemctl("disable", unit)
            install.status = "disabled"
            install.enabled_at = None
            install.service_status = None
            save_state(self._installs)
        log.info("plugin_disabled", plugin_id=plugin_id)

    def remove(self, plugin_id: str, *, keep_data: bool = False) -> None:
        # disable() takes the state_lock itself; call it outside the lock.
        install = self._require_install(plugin_id)
        if install.status in ("running", "enabled"):
            try:
                self.disable(plugin_id)
            except SupervisorError as exc:
                log.warning(
                    "plugin_disable_during_remove_failed",
                    plugin_id=plugin_id,
                    error=str(exc),
                )
        with state_lock():
            manifest = self._manifest_for(plugin_id)
            if (
                manifest.agent is not None
                and manifest.agent.isolation == "subprocess"
            ):
                # Delete the declared extra-service unit files alongside
                # the main runner unit. disable() already stopped them.
                for service in self._declared_services(manifest):
                    svc_unit_path = service_unit_path_for(
                        plugin_id, service.name
                    )
                    if svc_unit_path.exists():
                        svc_unit_path.unlink()
                unit_path = unit_path_for(plugin_id)
                if unit_path.exists():
                    unit_path.unlink()
                self._systemctl("daemon-reload")
            target = self._install_dir / plugin_id
            if target.exists():
                shutil.rmtree(target)
            if not keep_data:
                log_file = PLUGIN_LOG_DIR / f"{plugin_id.replace('.', '-')}.log"
                if log_file.exists():
                    log_file.unlink()
            self._installs = remove_install(self._installs, plugin_id)
            save_state(self._installs)
        log.info("plugin_removed", plugin_id=plugin_id, keep_data=keep_data)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _require_install(self, plugin_id: str) -> PluginInstall:
        install = find_install(self._installs, plugin_id)
        if install is None:
            raise SupervisorError(f"plugin {plugin_id} is not installed")
        return install

    def manifest_for(self, plugin_id: str) -> PluginManifest:
        """Public accessor: the manifest for an installed/built-in plugin (tamper-checked)."""
        return self._manifest_for(plugin_id)

    def _manifest_for(self, plugin_id: str) -> PluginManifest:
        # Built-in first; otherwise read from unpacked dir.
        builtin = self._builtin.get(plugin_id)
        if builtin is not None:
            return builtin
        manifest_path = self._install_dir / plugin_id / MANIFEST_FILENAME
        if not manifest_path.exists():
            raise SupervisorError(
                f"plugin {plugin_id} manifest missing at {manifest_path}"
            )
        manifest_bytes = manifest_path.read_bytes()
        # Manifest-hash tamper check: compare to recorded hash from install.
        install = find_install(self._installs, plugin_id)
        if install is not None:
            current_hash = hashlib.sha256(manifest_bytes).hexdigest()
            if install.manifest_hash and current_hash != install.manifest_hash:
                raise SupervisorError(
                    f"plugin {plugin_id} manifest hash mismatch; "
                    f"on-disk file has been modified since install"
                )
        return PluginManifest.from_yaml_text(manifest_bytes.decode("utf-8"))

    def _check_compatibility(
        self, manifest: PluginManifest, signer_id: str | None
    ) -> None:
        # ados_version: very simple containment check until we wire a real
        # semver-range parser. The manifest stores e.g. ">=0.9.0,<1.0.0";
        # for v0.1 we only verify the current agent version is non-empty
        # and emit a warning when the constraint string can't be parsed.
        agent_version = _ados.__version__
        constraint = manifest.compatibility.ados_version.strip()
        if not constraint:
            raise ManifestError(
                f"plugin {manifest.id} has empty compatibility.ados_version"
            )
        if not _semver_in_range(agent_version, constraint):
            raise SupervisorError(
                f"plugin {manifest.id} requires ADOS version {constraint}; "
                f"running {agent_version}"
            )
        if self._current_board_id and not manifest.compatibility.supports_board(
            self._current_board_id
        ):
            raise SupervisorError(
                f"plugin {manifest.id} does not support board "
                f"{self._current_board_id}"
            )
        # min_tier floor: refuse when the plugin needs a higher compute
        # class than the current board provides. Lenient when either the
        # floor or the board tier is unknown, matching supported_boards.
        min_tier = manifest.compatibility.min_tier
        if (
            min_tier is not None
            and self._current_board_tier is not None
            and self._current_board_tier < min_tier
        ):
            raise SupervisorError(
                f"plugin {manifest.id} requires compute tier {min_tier}; "
                f"this board is tier {self._current_board_tier}"
            )
        if (
            manifest.agent is not None
            and manifest.agent.isolation == "inprocess"
            and (signer_id is None or not is_first_party_signer(signer_id))
        ):
            raise SupervisorError(
                f"plugin {manifest.id} requests inprocess isolation but "
                f"signer {signer_id} is not first-party"
            )

    def _reject_downgrade(self, manifest: PluginManifest) -> None:
        """Refuse an archive whose version is lower than what is installed.

        An unparseable version on either side is not treated as a downgrade:
        the semver-range gate has already had its say, and refusing here over
        a formatting detail would block a legitimate install.
        """
        install = find_install(self._installs, manifest.id)
        if install is None:
            return
        try:
            incoming = _semver_tuple(manifest.version)
            installed = _semver_tuple(install.version)
        except SupervisorError:
            return
        if incoming < installed:
            raise SupervisorError(
                f"plugin {manifest.id} archive is version {manifest.version} "
                f"but {install.version} is installed; refusing a downgrade "
                f"(a signed older version is a rollback attack). Pass "
                f"--allow-downgrade to force it"
            )

    def _install_agent_wheels(self, manifest: PluginManifest, target: Path) -> None:
        """Install the agent half's vendored wheels into a per-plugin site dir.

        The archive layout documents ``agent/wheel/<name>-<ver>-*.whl`` and
        nothing installed it, so an agent half importing anything outside the
        agent venv died at entrypoint import — the developer saw a unit
        flapping with an ImportError and a documented wheel slot implying
        dependency support that did not exist.

        Wheels go to ``<install_dir>/<plugin_id>/agent/site``, which the
        runner prepends to ``sys.path``, so one plugin's dependency can never
        shadow another's or the agent's own. ``--no-deps`` is deliberate: a
        plugin vendors its full closure into the archive, because resolving
        dependencies at install time would mean a network fetch on a vehicle
        that may have no route, at the least convenient moment.

        A wheel that fails to install fails the install: a plugin whose
        dependencies are half-present is worse than one that never installed,
        because the failure surfaces later and somewhere else.
        """
        if manifest.agent is None:
            return
        wheel_dir = target / "agent" / "wheel"
        wheels = sorted(wheel_dir.glob("*.whl")) if wheel_dir.is_dir() else []
        if not wheels:
            return
        site_dir = target / "agent" / "site"
        site_dir.mkdir(parents=True, exist_ok=True)
        cmd = [
            sys.executable,
            "-m",
            "pip",
            "install",
            "--no-deps",
            "--no-index",
            "--quiet",
            "--target",
            str(site_dir),
            *[str(w) for w in wheels],
        ]
        try:
            proc = subprocess.run(
                cmd, capture_output=True, text=True, timeout=300
            )
        except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
            raise SupervisorError(
                f"plugin {manifest.id}: installing vendored wheels failed: {exc}"
            ) from exc
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout or "").strip()[:500]
            raise SupervisorError(
                f"plugin {manifest.id}: installing vendored wheels failed "
                f"(exit {proc.returncode}): {detail}"
            )
        log.info(
            "plugin_wheels_installed",
            plugin_id=manifest.id,
            count=len(wheels),
            site_dir=str(site_dir),
        )

    def _reject_inline_for_third_party(
        self, manifest: PluginManifest, signer_id: str | None
    ) -> None:
        if (
            manifest.gcs is not None
            and manifest.gcs.isolation == "inline"
            and (signer_id is None or not is_first_party_signer(signer_id))
        ):
            raise SupervisorError(
                f"plugin {manifest.id} requests inline GCS isolation but "
                f"signer {signer_id} is not first-party"
            )

    def _ensure_slice_exists(self) -> None:
        slice_path = _systemd.PLUGIN_SLICE_PATH
        if slice_path.exists():
            return
        slice_path.parent.mkdir(parents=True, exist_ok=True)
        slice_path.write_text(slice_unit_content(), encoding="utf-8")
        self._systemctl("daemon-reload")

    # ------------------------------------------------------------------
    # Declared extra services (additive to the main runner unit)
    # ------------------------------------------------------------------

    def _declared_services(self, manifest: PluginManifest):
        """The list of ``ServiceSpec`` a plugin declares, or empty.

        Only subprocess agent halves get extra units; an inprocess
        built-in has no systemd footprint to attach services to.
        """
        agent = manifest.agent
        if agent is None or agent.isolation != "subprocess":
            return []
        return list(agent.contributes.services)

    def _start_declared_services(
        self, plugin_id: str, manifest: PluginManifest
    ) -> None:
        """Render + enable + start one unit per declared service.

        Best-effort per service: a render or systemctl failure on one
        service is logged and left to surface as not-ready(reason) on
        the readiness probe; it never aborts the enable of the plugin's
        main half."""
        for service in self._declared_services(manifest):
            try:
                svc_unit_path = service_unit_path_for(plugin_id, service.name)
                svc_unit_path.write_text(
                    render_service_unit(manifest, service, self._install_dir),
                    encoding="utf-8",
                )
                self._systemctl("daemon-reload")
                unit = service_unit_name_for(plugin_id, service.name)
                self._systemctl("enable", unit)
                self._systemctl("start", unit)
            except (SupervisorError, OSError, ValueError) as exc:
                log.warning(
                    "plugin_service_start_failed",
                    plugin_id=plugin_id,
                    service=service.name,
                    error=str(exc),
                )

    def _stop_declared_services(
        self, plugin_id: str, manifest: PluginManifest
    ) -> None:
        """Stop + disable each declared service unit. Best-effort."""
        for service in self._declared_services(manifest):
            unit = service_unit_name_for(plugin_id, service.name)
            for verb in ("stop", "disable"):
                try:
                    self._systemctl(verb, unit)
                except SupervisorError as exc:
                    log.warning(
                        "plugin_service_stop_failed",
                        plugin_id=plugin_id,
                        service=service.name,
                        verb=verb,
                        error=str(exc),
                    )

    def _compute_service_readiness(
        self, plugin_id: str, manifest: PluginManifest
    ) -> list[dict] | None:
        """Probe each declared service and return readiness entries.

        Returns ``None`` when the plugin declares no services so the
        heartbeat omits the block entirely. Each entry is
        ``{"name", "ready": bool, "reason": str | None}``.

        Readiness rules per service:

        * ``ready_check`` absent ⇒ ready iff the unit is active.
        * ``ready_check`` an ``http(s)://127.0.0.1:<port>`` URL ⇒ ready on
          a 2xx GET.
        * ``ready_check`` anything else ⇒ ready on an argv exiting 0, run
          sandboxed as the plugin user (:meth:`_probe_command_ready`).

        All probes are short-timeout and never raise into the caller;
        a probe error becomes ``ready=False`` with the error as the
        reason.
        """
        services = self._declared_services(manifest)
        if not services:
            return None
        out: list[dict] = []
        for service in services:
            ready, reason = self._probe_service_ready(plugin_id, manifest, service)
            out.append({"name": service.name, "ready": ready, "reason": reason})
        return out

    def _probe_service_ready(
        self, plugin_id: str, manifest: PluginManifest, service
    ) -> tuple[bool, str | None]:
        """Probe one service's readiness. ``service`` is a ServiceSpec."""
        check = service.ready_check
        if check is None:
            active = self._unit_is_active(
                service_unit_name_for(plugin_id, service.name)
            )
            return (active, None if active else "unit not active")
        try:
            probe = parse_ready_check(check)
        except ValueError as exc:
            return (False, f"invalid ready_check: {exc}")
        if isinstance(probe, HttpProbe):
            return self._probe_http_ready(probe.url)
        return self._probe_command_ready(manifest, probe.argv)

    def _unit_is_active(self, unit: str) -> bool:
        """``systemctl is-active --quiet <unit>`` ⇒ exit 0 means active."""
        try:
            proc = subprocess.run(
                ["systemctl", "is-active", "--quiet", unit],
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return False
        return proc.returncode == 0

    def _probe_http_ready(self, url: str) -> tuple[bool, str | None]:
        """HTTP GET; ready on a 2xx status."""
        try:
            with urllib.request.urlopen(url, timeout=5) as resp:  # noqa: S310
                status = getattr(resp, "status", None) or resp.getcode()
            if 200 <= int(status) < 300:
                return (True, None)
            return (False, f"http status {status}")
        except urllib.error.HTTPError as exc:
            return (False, f"http status {exc.code}")
        except (urllib.error.URLError, ValueError, OSError) as exc:
            return (False, f"http probe failed: {exc}")

    def _probe_command_ready(
        self, manifest: PluginManifest, argv: tuple[str, ...]
    ) -> tuple[bool, str | None]:
        """Run a declared readiness argv; ready on exit 0.

        The argv is plugin-authored, so it never runs in this process and
        never through a shell: ``systemd-run`` starts it as a transient unit
        with the ``ados`` user and the same sandbox and resource envelope the
        plugin's declared services get (:func:`probe_command`).
        """
        command = probe_command(manifest, argv, self._install_dir)
        try:
            proc = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=PROBE_TIMEOUT_S + 5,
            )
        except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
            return (False, f"command probe failed: {exc}")
        if proc.returncode == 0:
            return (True, None)
        detail = (proc.stderr or proc.stdout or "").strip()
        reason = f"exit {proc.returncode}"
        if detail:
            reason = f"{reason}: {detail[:200]}"
        return (False, reason)

    def readiness_for(self, plugin_id: str) -> list[dict]:
        """Public read: probe the declared services of an installed
        plugin and return the readiness entries (empty list when none).

        Reuses the same probe path the enable flow persists. Raises
        :class:`SupervisorError` if the plugin is not installed.
        """
        self._require_install(plugin_id)
        manifest = self._manifest_for(plugin_id)
        return self._compute_service_readiness(plugin_id, manifest) or []

    def _systemctl(self, *args: str) -> None:
        try:
            subprocess.run(
                ["systemctl", *args],
                check=True,
                capture_output=True,
                text=True,
                timeout=15,
            )
        except FileNotFoundError as exc:
            raise SupervisorError(
                "systemctl not found; is this a systemd host?"
            ) from exc
        except subprocess.CalledProcessError as exc:
            raise SupervisorError(
                f"systemctl {' '.join(args)} failed: {exc.stderr.strip()}"
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise SupervisorError(
                f"systemctl {' '.join(args)} timed out"
            ) from exc


def _now_ms() -> int:
    import time

    return int(time.time() * 1000)


def _semver_in_range(version: str, constraint: str) -> bool:
    """Bounded semver-range parser for the v0.1 constraint vocabulary.

    Supports comma-separated atoms each of the form ``<op><semver>`` where
    op is one of ``>=``, ``<=``, ``>``, ``<``, ``==``, ``=``. Implicit AND
    across atoms. Bare ``<semver>`` means ``==<semver>``.
    """
    parts = [p.strip() for p in constraint.split(",") if p.strip()]
    cur = _semver_tuple(version)
    for atom in parts:
        op, target = _split_op(atom)
        tt = _semver_tuple(target)
        if op == "==" or op == "=":
            if cur != tt:
                return False
        elif op == ">=":
            if cur < tt:
                return False
        elif op == "<=":
            if cur > tt:
                return False
        elif op == ">":
            if cur <= tt:
                return False
        elif op == "<":
            if cur >= tt:
                return False
        else:
            return False
    return True


def _split_op(atom: str) -> tuple[str, str]:
    for op in (">=", "<=", "==", ">", "<", "="):
        if atom.startswith(op):
            return op, atom[len(op) :].strip()
    return "==", atom


def _semver_tuple(v: str) -> tuple[int, int, int]:
    base = v.split("-", 1)[0].split("+", 1)[0]
    parts = base.split(".")
    if len(parts) < 3:
        parts = (parts + ["0", "0", "0"])[:3]
    try:
        return (int(parts[0]), int(parts[1]), int(parts[2]))
    except ValueError as exc:
        raise SupervisorError(f"unparseable semver {v}") from exc
