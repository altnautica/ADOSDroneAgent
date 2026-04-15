"""share_uplink firewall and sysctl persistence.

Phase 3 wired runtime sysctl + iptables MASQUERADE for `share_uplink`
through `api/routes/ground_station.py:_apply_share_uplink`. Those
settings did NOT survive reboot. Phase 4 Wave 2 Cellos adds proper
persistence here:

- Atomic write of `/etc/sysctl.d/99-ados-share-uplink.conf` for
  `net.ipv4.ip_forward=1` (or removal of that file when disabled).
- iptables-persistent (Debian) save to `/etc/iptables/rules.v4` after
  every rule change.
- nftables fallback that rewrites `/etc/nftables.conf` from
  `nft list ruleset`.
- Refuse-and-log when neither iptables nor nftables is available.
- Idempotent reconcile-on-start that brings runtime state into agreement
  with the persisted `ground_station.share_uplink` flag.

Called by `uplink_router._run_service()` at startup and by the
`PUT /network/share_uplink` route on transitions.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import tempfile
from pathlib import Path
from typing import Optional

import structlog

log = structlog.get_logger("ground_station.share_uplink_firewall")


SYSCTL_DROPIN_PATH = Path("/etc/sysctl.d/99-ados-share-uplink.conf")
IPTABLES_RULES_V4_PATH = Path("/etc/iptables/rules.v4")
NFTABLES_CONF_PATH = Path("/etc/nftables.conf")
OS_RELEASE_PATH = Path("/etc/os-release")


# ----------------------------------------------------------------------
# Backend detection
# ----------------------------------------------------------------------
def _detect_distro_id() -> str:
    """Return the lowercase ID from /etc/os-release, or 'unknown'."""
    try:
        if not OS_RELEASE_PATH.exists():
            return "unknown"
        for line in OS_RELEASE_PATH.read_text().splitlines():
            if line.startswith("ID="):
                return line.split("=", 1)[1].strip().strip('"').lower()
    except Exception:
        pass
    return "unknown"


def _have_iptables() -> bool:
    return shutil.which("iptables") is not None


def _have_iptables_persistent() -> bool:
    """True when /etc/iptables/ exists. iptables-persistent owns it."""
    return Path("/etc/iptables").is_dir()


def _have_nftables() -> bool:
    return shutil.which("nft") is not None


def detect_firewall_backend() -> str:
    """Pick the best available persistence backend.

    Returns one of: 'iptables-persistent', 'nftables', 'iptables-runtime',
    'none'. 'iptables-runtime' means iptables is available but no
    persistence dir; runtime rules apply but do not survive reboot.
    """
    if _have_iptables() and _have_iptables_persistent():
        return "iptables-persistent"
    if _have_nftables():
        return "nftables"
    if _have_iptables():
        return "iptables-runtime"
    return "none"


# ----------------------------------------------------------------------
# Atomic file helpers
# ----------------------------------------------------------------------
def _atomic_write(path: Path, content: str) -> None:
    """Temp+rename so partial writes never land on disk."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(prefix=path.name + ".", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w") as f:
            f.write(content)
        os.replace(tmp_path, path)
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


# ----------------------------------------------------------------------
# sysctl persistence
# ----------------------------------------------------------------------
def write_sysctl_dropin() -> None:
    """Persist net.ipv4.ip_forward=1 across reboots."""
    body = (
        "# Managed by ADOS share_uplink. Do not edit by hand.\n"
        "net.ipv4.ip_forward=1\n"
    )
    _atomic_write(SYSCTL_DROPIN_PATH, body)


def remove_sysctl_dropin() -> None:
    try:
        SYSCTL_DROPIN_PATH.unlink()
    except FileNotFoundError:
        pass


async def _run(*cmd: str) -> tuple[int, str, str]:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    out, err = await proc.communicate()
    return (
        proc.returncode if proc.returncode is not None else -1,
        out.decode(errors="replace").strip(),
        err.decode(errors="replace").strip(),
    )


async def apply_sysctl_runtime(enabled: bool) -> Optional[str]:
    """Apply ip_forward at runtime via `sysctl -w`. Returns error or None."""
    rc, _out, err = await _run(
        "sysctl", "-w", f"net.ipv4.ip_forward={1 if enabled else 0}"
    )
    if rc != 0:
        return err or "sysctl_failed"
    return None


# ----------------------------------------------------------------------
# iptables-persistent backend
# ----------------------------------------------------------------------
async def _iptables_rule_present(iface: str) -> bool:
    rc, _o, _e = await _run(
        "iptables", "-t", "nat", "-C", "POSTROUTING",
        "-o", iface, "-j", "MASQUERADE",
    )
    return rc == 0


async def _iptables_add_rule(iface: str) -> Optional[str]:
    rc, _o, err = await _run(
        "iptables", "-t", "nat", "-A", "POSTROUTING",
        "-o", iface, "-j", "MASQUERADE",
    )
    if rc != 0:
        return err or "iptables_add_failed"
    return None


async def _iptables_remove_rule(iface: str) -> Optional[str]:
    if not await _iptables_rule_present(iface):
        return None
    rc, _o, err = await _run(
        "iptables", "-t", "nat", "-D", "POSTROUTING",
        "-o", iface, "-j", "MASQUERADE",
    )
    if rc != 0:
        return err or "iptables_remove_failed"
    return None


async def _iptables_save() -> Optional[str]:
    """Persist current rules to /etc/iptables/rules.v4 atomically."""
    rc, out, err = await _run("iptables-save")
    if rc != 0:
        return err or "iptables_save_failed"
    try:
        IPTABLES_RULES_V4_PATH.parent.mkdir(parents=True, exist_ok=True)
        _atomic_write(IPTABLES_RULES_V4_PATH, out + "\n")
    except OSError as exc:
        return f"iptables_save_write_failed: {exc}"
    return None


# ----------------------------------------------------------------------
# nftables backend
# ----------------------------------------------------------------------
_NFT_TABLE = "ados_nat"
_NFT_CHAIN = "postrouting"


async def _nft_ensure_table_chain() -> Optional[str]:
    rc, _o, err = await _run("nft", "add", "table", "ip", _NFT_TABLE)
    if rc != 0 and "exists" not in err.lower():
        return err or "nft_table_failed"
    rc, _o, err = await _run(
        "nft", "add", "chain", "ip", _NFT_TABLE, _NFT_CHAIN,
        "{", "type", "nat", "hook", "postrouting", "priority", "100", ";", "}",
    )
    if rc != 0 and "exists" not in err.lower():
        return err or "nft_chain_failed"
    return None


async def _nft_rule_present(iface: str) -> bool:
    rc, out, _e = await _run(
        "nft", "list", "chain", "ip", _NFT_TABLE, _NFT_CHAIN,
    )
    if rc != 0:
        return False
    return f'oifname "{iface}"' in out and "masquerade" in out


async def _nft_add_rule(iface: str) -> Optional[str]:
    err = await _nft_ensure_table_chain()
    if err is not None:
        return err
    if await _nft_rule_present(iface):
        return None
    rc, _o, err = await _run(
        "nft", "add", "rule", "ip", _NFT_TABLE, _NFT_CHAIN,
        "oifname", iface, "masquerade",
    )
    if rc != 0:
        return err or "nft_add_failed"
    return None


async def _nft_remove_rule(iface: str) -> Optional[str]:
    """Flush our table chain rather than hunt by handle. Cheaper, idempotent."""
    rc, _o, err = await _run(
        "nft", "flush", "chain", "ip", _NFT_TABLE, _NFT_CHAIN,
    )
    if rc != 0 and "no such" not in err.lower():
        return err or "nft_flush_failed"
    return None


async def _nft_save() -> Optional[str]:
    rc, out, err = await _run("nft", "list", "ruleset")
    if rc != 0:
        return err or "nft_save_failed"
    try:
        _atomic_write(NFTABLES_CONF_PATH, out + "\n")
    except OSError as exc:
        return f"nft_save_write_failed: {exc}"
    return None


# ----------------------------------------------------------------------
# Public entry points
# ----------------------------------------------------------------------
async def apply_share_uplink(enabled: bool, active_iface: Optional[str]) -> dict:
    """Apply or remove sysctl + NAT MASQUERADE and persist to disk.

    Returns a dict {applied: bool, backend: str, apply_error: str|None}.
    Always best-effort: never raises. The caller persists the config
    flag separately so a transient firewall failure does not desync.
    """
    backend = detect_firewall_backend()
    if backend == "none":
        msg = "no_firewall_backend (neither iptables nor nftables found)"
        log.error("share_uplink.no_backend", error=msg)
        return {"applied": False, "backend": backend, "apply_error": msg}

    apply_error: Optional[str] = None

    # --- sysctl -----------------------------------------------------
    rt_err = await apply_sysctl_runtime(enabled)
    if rt_err:
        apply_error = rt_err
    try:
        if enabled:
            write_sysctl_dropin()
        else:
            remove_sysctl_dropin()
    except OSError as exc:
        apply_error = apply_error or f"sysctl_dropin_failed: {exc}"

    # --- NAT --------------------------------------------------------
    if active_iface:
        if backend in ("iptables-persistent", "iptables-runtime"):
            if enabled:
                if not await _iptables_rule_present(active_iface):
                    err = await _iptables_add_rule(active_iface)
                    if err:
                        apply_error = apply_error or err
            else:
                err = await _iptables_remove_rule(active_iface)
                if err:
                    apply_error = apply_error or err
            if backend == "iptables-persistent":
                save_err = await _iptables_save()
                if save_err:
                    apply_error = apply_error or save_err
            else:
                log.warning(
                    "share_uplink.iptables_no_persistence",
                    note=(
                        "iptables-persistent not installed. Rules apply "
                        "now but will NOT survive reboot. Install with "
                        "ADOS_ENABLE_SHARE_UPLINK=1 reinstall."
                    ),
                )
        elif backend == "nftables":
            if enabled:
                err = await _nft_add_rule(active_iface)
                if err:
                    apply_error = apply_error or err
            else:
                err = await _nft_remove_rule(active_iface)
                if err:
                    apply_error = apply_error or err
            save_err = await _nft_save()
            if save_err:
                apply_error = apply_error or save_err
    elif enabled:
        log.warning("share_uplink.no_active_iface", note="ip_forward set but no NAT rule")

    log.info(
        "share_uplink.apply_done",
        enabled=enabled,
        iface=active_iface,
        backend=backend,
        error=apply_error,
    )
    return {
        "applied": apply_error is None,
        "backend": backend,
        "apply_error": apply_error,
    }


async def reconcile_on_start() -> dict:
    """Reconcile firewall state against the persisted share_uplink flag.

    Called by `ados-uplink-router.service` on start. Reads the current
    `ground_station.share_uplink` value and verifies sysctl + NAT match.
    Drift triggers a re-apply.
    """
    try:
        from ados.core.config import load_config
        cfg = load_config()
        enabled = bool(getattr(cfg.ground_station, "share_uplink", False))
    except Exception as exc:
        log.warning("share_uplink.reconcile_config_load_failed", error=str(exc))
        return {"reconciled": False, "error": str(exc)}

    # Discover the active uplink iface. Best-effort only.
    active_iface: Optional[str] = None
    try:
        from ados.services.ground_station.uplink_router import get_uplink_router
        router = get_uplink_router()
        active_name = router.active_uplink
        if active_name:
            mgr = await router._manager_for(active_name)  # type: ignore[attr-defined]
            if mgr is not None:
                get_iface = getattr(mgr, "get_iface", None)
                if callable(get_iface):
                    active_iface = get_iface()
    except Exception as exc:
        log.debug("share_uplink.reconcile_iface_lookup_failed", error=str(exc))

    log.info(
        "share_uplink.reconcile_start",
        configured=enabled,
        iface=active_iface,
    )
    result = await apply_share_uplink(enabled, active_iface)
    return {
        "reconciled": True,
        "configured_enabled": enabled,
        "iface": active_iface,
        **result,
    }
