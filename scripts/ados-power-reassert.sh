#!/bin/sh
# =============================================================================
# ados-power-reassert.sh — re-assert runtime power knobs after a cold boot.
#
# udev RUN+= rules cover hotplug, but a device present at boot can win the
# race before its rule is loaded, and a NetworkManager reload does not touch
# a raw wlan brought up outside NM. This oneshot (ados-power.service) sweeps
# every wlan* power_save and every USB power/control once at boot.
#
# Forgiving by design: every step is best-effort and the script always
# exits 0 so a missing device or knob never fails the boot target.
# =============================================================================

for _ifdir in /sys/class/net/wlan*; do
    [ -e "${_ifdir}" ] || continue
    _if="$(basename "${_ifdir}")"
    iw dev "${_if}" set power_save off 2>/dev/null || true
done

for _ctl in /sys/bus/usb/devices/*/power/control; do
    [ -w "${_ctl}" ] || continue
    echo on > "${_ctl}" 2>/dev/null || true
done

# Ethernet EEE off — but NEVER the interface that owns the default route.
# Changing EEE renegotiates the PHY (a brief link bounce); on some Rockchip
# GbE PHYs the wired link does not recover, which would cut off a rig reached
# over Ethernet. So skip the management NIC and only touch other wired ports.
_def_if="$(ip route show default 2>/dev/null | awk '{print $5; exit}')"
for _ed in /sys/class/net/eth* /sys/class/net/end* /sys/class/net/enP* /sys/class/net/enx*; do
    [ -e "${_ed}" ] || continue
    _eif="$(basename "${_ed}")"
    [ "${_eif}" = "${_def_if}" ] && continue
    ethtool --set-eee "${_eif}" eee off 2>/dev/null || true
done

exit 0
