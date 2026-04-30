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
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import (
    PLUGINS_INSTALL_DIR,
    PLUGIN_LOG_DIR,
    PLUGIN_RUN_DIR,
)
from ados.plugins.archive import (
    MANIFEST_FILENAME,
    open_archive,
    unpack_to,
)
from ados.plugins.errors import (
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.loader import load_builtin_manifests
from ados.plugins.manifest import PluginManifest
from ados.plugins.signing import (
    is_first_party_signer,
    verify_archive_signature,
)
from ados.plugins.state import (
    PluginInstall,
    filter_permissions_against_manifest,
    find_install,
    grant_permission,
    load_state,
    remove_install,
    save_state,
    state_lock,
    upsert_install,
)
from ados.plugins import systemd as _systemd
from ados.plugins.systemd import (
    render_unit,
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
    ) -> None:
        self._install_dir = install_dir or PLUGINS_INSTALL_DIR
        self._require_signed = require_signed
        self._current_board_id = current_board_id
        self._installs: list[PluginInstall] = []
        self._builtin: dict[str, PluginManifest] = {}

    # ------------------------------------------------------------------
    # Boot-time discovery
    # ------------------------------------------------------------------

    def discover(self) -> None:
        """Read on-disk state, load built-in entry-points, sanity-check.

        Also filters in-memory permission grants down to what the
        manifest currently declares, defending against a tampered
        state file (security audit finding #5).
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

    # ------------------------------------------------------------------
    # Install / enable / disable / remove
    # ------------------------------------------------------------------

    def install_archive(self, archive_path: Path) -> InstallResult:
        """Install a ``.adosplug`` archive. Returns a summary.

        Caller is responsible for prompting the operator to approve
        permissions BEFORE calling this method. The supervisor records
        every requested permission as ``granted=False`` initially; the
        operator-side flow then calls :meth:`grant_permission` per
        approved permission.

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

        with state_lock():
            target = self._install_dir / manifest.id
            if target.exists():
                shutil.rmtree(target)
            unpack_to(contents.raw_archive_bytes, target)

            # Write systemd unit for subprocess agent halves.
            if (
                manifest.agent is not None
                and manifest.agent.isolation == "subprocess"
            ):
                self._ensure_slice_exists()
                unit_path = unit_path_for(manifest.id)
                unit_path.write_text(render_unit(manifest), encoding="utf-8")
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
            unit = unit_name_for(plugin_id)
            self._systemctl("enable", unit)
            self._systemctl("start", unit)
            install.status = "running"
            install.enabled_at = _now_ms()
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
                unit = unit_name_for(plugin_id)
                self._systemctl("stop", unit)
                self._systemctl("disable", unit)
            install.status = "disabled"
            install.enabled_at = None
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
        if (
            manifest.compatibility.supported_boards
            and self._current_board_id
            and self._current_board_id
            not in manifest.compatibility.supported_boards
        ):
            raise SupervisorError(
                f"plugin {manifest.id} does not support board "
                f"{self._current_board_id}"
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
