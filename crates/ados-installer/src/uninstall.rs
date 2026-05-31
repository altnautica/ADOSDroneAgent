//! Full uninstall / purge path + GS→drone residue reversion.
//!
//! Mirrors the bash `do_uninstall` / `purge_ados_artifacts`: stop + disable +
//! remove every `ados-*` unit, the dropins (tmpfiles/sysctl/udev/modules-load/
//! NetworkManager/logind/avahi), the `/usr/local/bin/ados*` symlinks, and the
//! `/opt/ados`, `/var/ados`, `/var/lib/ados`, `/var/log/ados`, `/run/ados`
//! trees; with `purge`, also `/etc/ados`. Shares the residue reversion in
//! [`crate::steps::purge_residue`] (the orphan `default dev eth0 scope link`
//! route and the `/boot/firmware/config.txt` LCD overlay) so a GS→drone flip
//! leaves a clean box.
//!
//! Implementation lands in the leaf-module phase.

/// Run the uninstall. `purge` additionally removes `/etc/ados` (device id,
/// pairing, config) for a from-clean reinstall.
pub fn run_uninstall(_purge: bool) -> anyhow::Result<()> {
    anyhow::bail!("uninstall::run_uninstall is implemented in the leaf-module phase")
}
