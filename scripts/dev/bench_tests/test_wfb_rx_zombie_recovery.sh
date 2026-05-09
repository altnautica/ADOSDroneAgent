#!/usr/bin/env bash
# test_wfb_rx_zombie_recovery.sh — exercise the GS-side RX-liveness watchdog.
#
# Symmetric to test_wfb_tx_zombie_recovery.sh but the success signal
# is different: the wlan iface rx_packets counter is NOT a wfb_rx
# liveness signal because the kernel keeps capturing 802.11 frames in
# monitor mode regardless of whether the userspace consumer is
# running. We assert respawn instead by tracking the wfb_rx PID and
# confirming a new PID appears within the watchdog + restart window.
#
# Usage: ./test_wfb_rx_zombie_recovery.sh <ssh-target> <ssh-pass>
#   e.g. ./test_wfb_rx_zombie_recovery.sh skynode@skynode.local root

set -u
TARGET="${1:?usage: $0 <ssh-target> <ssh-pass>}"
PASS="${2:?usage: $0 <ssh-target> <ssh-pass>}"

run_remote() {
  ssh -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new "$TARGET" "$@"
}

run_sudo() {
  run_remote "echo '$PASS' | sudo -S $1 2>&1"
}

echo "=== test_wfb_rx_zombie_recovery on $TARGET ==="
PID=$(run_remote 'pgrep -f "^wfb_rx -p 0" | head -1' | tr -d '\r\n')
if [ -z "$PID" ]; then
  echo "FAIL: wfb_rx not running (start ados-wfb-rx first)"
  exit 1
fi
echo "wfb_rx pid=$PID"

# Pull the iface name from wfb_rx's own argv (last positional) so we
# never test the wrong adapter. Pi rigs typically have wlan0 (onboard
# WiFi) AND wlan1/wlxXXXX (the RTL8812AU in monitor mode).
IFACE=$(run_remote "tr '\\0' ' ' < /proc/$PID/cmdline 2>/dev/null | awk '{print \$NF}'" | tr -d '\r\n ')
echo "iface=$IFACE"

echo "freezing wfb_rx with SIGSTOP (the zombie pattern)..."
run_sudo "kill -STOP $PID" >/dev/null

# Watchdog: wfb_rx stdout silent for 30 s, then terminate, then ~5-10 s
# for the standard restart loop to respawn. Allow 70 s total.
echo "waiting up to 70 s for watchdog + respawn (new PID = success)..."
SUCCESS=0
for i in $(seq 1 14); do
  sleep 5
  NEW_PID=$(run_remote 'pgrep -f "^wfb_rx -p 0" | head -1' | tr -d '\r\n')
  echo "  t=$((i*5))s pid=$NEW_PID (original=$PID)"
  if [ -n "$NEW_PID" ] && [ "$NEW_PID" != "$PID" ]; then
    echo "PASS: watchdog respawned wfb_rx ($PID -> $NEW_PID)"
    SUCCESS=1
    break
  fi
done

if [ $SUCCESS -ne 1 ]; then
  # Cleanup: unfreeze the original PID so the rig isn't left wedged
  run_sudo "kill -CONT $PID" >/dev/null 2>&1 || true
  echo "FAIL: wfb_rx did not respawn within 70 s of SIGSTOP"
  echo "  last seen pid: $NEW_PID (original $PID)"
  echo "  watchdog state on rig:"
  run_remote "echo '$PASS' | sudo -S journalctl -u ados-wfb-rx -n 30 --since '2 minutes ago' 2>/dev/null | grep -iE 'zombie|rx_health|rx_silent' | tail -10"
  exit 2
fi

echo "=== test_wfb_rx_zombie_recovery PASSED ==="
exit 0
