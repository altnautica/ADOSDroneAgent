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

exit 0
