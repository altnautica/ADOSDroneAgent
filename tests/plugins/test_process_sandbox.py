"""Sandbox guards for a plugin spawning a vendored binary.

`process_sandbox` is the boundary between a plugin's manifest-declared vendor
binaries and `subprocess`: it resolves a basename to a path inside the plugin's
own install directory and refuses anything that escapes it or that the
manifest did not authorize. It is imported by `ados.plugins.runner`, the
`ados-plugin-runner` console script the native host execs, so it ships on every
node that runs a plugin.

These tests previously lived alongside the packaged IPC host's tests and were
removed with it. They never touched that host — they cover a module that is
still live — so they are restored here, in a file that imports nothing from
the deleted island.
"""

from __future__ import annotations

import stat
from pathlib import Path

import pytest

from ados.plugins.process_sandbox import (
    AllowlistViolation,
    SpawnError,
    resolve_binary,
)
from ados.plugins.process_sandbox import (
    spawn as sandbox_spawn,
)


def _make_fake_vendor_binary(install_dir: Path, basename: str) -> Path:
    """A minimal executable under the plugin's own ``vendor/`` directory."""
    vendor = install_dir / "vendor"
    vendor.mkdir(parents=True, exist_ok=True)
    path = vendor / basename
    path.write_text("#!/bin/sh\necho ok\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


def test_resolve_binary_rejects_traversal(tmp_path: Path) -> None:
    """A basename must not be able to climb out of the install directory.

    Asserts the REASON, not merely that something raised. Two independent
    guards can reject this one — the basename filter and the resolved-path
    escape check — and either is a legitimate defence, but "it raised" alone
    also covers "the file happened not to exist", which is not a guard at all.
    """
    with pytest.raises(SpawnError) as excinfo:
        resolve_binary(tmp_path, "../../etc/passwd")
    message = str(excinfo.value)
    assert "unsafe binary basename" in message or "escapes vendor root" in message, (
        f"rejected for the wrong reason: {message!r}"
    )


def test_resolve_binary_rejects_shell_meta(tmp_path: Path) -> None:
    """A basename carrying shell metacharacters is rejected as a NAME.

    This one has to name the basename guard specifically. `vendor;rm -rf /`
    resolves to a path inside the vendor root, so the escape check does not
    fire and the only other thing that raises is "not found" — which would
    also raise for an innocent typo. Verified: deleting the basename guard
    leaves a bare `pytest.raises(SpawnError)` green, so that form of the test
    proves nothing about whether the name was validated.
    """
    with pytest.raises(SpawnError) as excinfo:
        resolve_binary(tmp_path, "vendor;rm -rf /")
    assert "unsafe binary basename" in str(excinfo.value), (
        "the name must be rejected as unsafe, not merely fail to exist: "
        f"{excinfo.value!s}"
    )


def test_sandbox_spawn_denies_off_allowlist(tmp_path: Path) -> None:
    """A binary present on disk but absent from the manifest allowlist is refused."""
    _make_fake_vendor_binary(tmp_path, "ok-bin")
    with pytest.raises(AllowlistViolation):
        sandbox_spawn(
            plugin_id="p",
            install_dir=tmp_path,
            allowlist=frozenset({"ok-bin"}),
            basename="other-bin",
        )


def test_sandbox_spawn_runs_real_binary(tmp_path: Path) -> None:
    """The allowed path still works: an authorized binary spawns and exits 0."""
    _make_fake_vendor_binary(tmp_path, "ok-bin")
    proc = sandbox_spawn(
        plugin_id="p",
        install_dir=tmp_path,
        allowlist=frozenset({"ok-bin"}),
        basename="ok-bin",
    )
    try:
        assert proc.wait(timeout=5.0) == 0
    finally:
        proc.terminate()


def test_plugin_context_exposes_the_v11_facades() -> None:
    """The `ctx` surface a plugin author writes against must keep its shape.

    Every facade is assigned in ``__init__`` rather than declared on the class,
    so this reads the constructor's attribute names instead of using ``hasattr``
    on the type — ``hasattr`` is ``False`` for all thirteen and would make the
    check either fail outright or, if papered over, assert nothing.

    That is what happened to the version of this test that used to live beside
    the packaged host's tests: its assertion ended in ``or True`` and its own
    comment conceded "the import is the test". Restoring it verbatim would have
    restored a test that cannot fail.
    """
    from ados.plugins.runner import PluginContext

    assigned = set(PluginContext.__init__.__code__.co_names)
    missing = [
        attr
        for attr in (
            "plugin_id",
            "plugin_version",
            "config",
            "agent_id",
            "log",
            "events",
            "mavlink",
            "peripheral_manager",
            "peripherals",
            "telemetry",
            "config_kv",
            "process",
            "lifecycle",
        )
        if attr not in assigned
    ]
    assert not missing, f"PluginContext no longer sets: {missing}"
