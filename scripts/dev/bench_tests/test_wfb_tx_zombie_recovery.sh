#!/usr/bin/env bash
# test_wfb_tx_zombie_recovery.sh — exercise the TX-liveness watchdog.
#
# How: SSH to the drone rig, SIGSTOP wfb_tx (process stays alive but
# stops scheduling — kernel still holds the iface, but the userspace
# loop never reads UDP 5600 again, so /sys tx_bytes stops incrementing).
# Then poll the radio iface tx_bytes counter and assert that within
# 60 s it starts incrementing again — proof the watchdog killed the
# stopped process and the supervisor's restart loop respawned it.
#
# Usage: ./test_wfb_tx_zombie_recovery.sh <ssh-target> <ssh-pass>
#   e.g. ./test_wfb_tx_zombie_recovery.sh radxa@groundnode.local radxa
#
# Exits 0 on success, non-zero on failure. No operator interaction.

set -u
TARGET="${1:?usage: $0 <ssh-target> <ssh-pass>}"
PASS="${2:?usage: $0 <ssh-target> <ssh-pass>}"

run_remote() {
  ssh -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new "$TARGET" "$@"
}

run_sudo() {
  run_remote "echo '$PASS' | sudo -S $1 2>&1"
}

echo "=== test_wfb_tx_zombie_recovery on $TARGET ==="
IFACE=$(run_remote 'ls /sys/class/net | grep -E "^wlx" | head -1' | tr -d '\r\n')
if [ -z "$IFACE" ]; then
  echo "FAIL: no RTL adapter (wlx*) found on $TARGET"
  exit 1
fi
echo "iface=$IFACE"

PID=$(run_remote 'pgrep -f "^wfb_tx -p 0" | head -1' | tr -d '\r\n')
if [ -z "$PID" ]; then
  echo "FAIL: wfb_tx not running on $TARGET (start ados-wfb first)"
  exit 1
fi
echo "wfb_tx pid=$PID"

BEFORE=$(run_remote "cat /sys/class/net/$IFACE/statistics/tx_bytes" | tr -d '\r\n')
echo "tx_bytes baseline=$BEFORE"

echo "freezing wfb_tx with SIGSTOP (the zombie pattern)..."
run_sudo "kill -STOP $PID" >/dev/null

# Watchdog tunables: poll every 5 s, threshold 30 s. Plus restart time
# (~5-10 s for the systemd respawn loop). Give it 60 s total.
echo "waiting up to 70 s for watchdog + respawn..."
SUCCESS=0
for i in $(seq 1 14); do
  sleep 5
  CURRENT=$(run_remote "cat /sys/class/net/$IFACE/statistics/tx_bytes" | tr -d '\r\n')
  NEW_PID=$(run_remote 'pgrep -f "^wfb_tx -p 0" | head -1' | tr -d '\r\n')
  echo "  t=$((i*5))s tx_bytes=$CURRENT pid=$NEW_PID"
  if [ -n "$NEW_PID" ] && [ "$NEW_PID" != "$PID" ] && [ "$CURRENT" -gt "$BEFORE" ]; then
    echo "PASS: watchdog respawned wfb_tx ($PID -> $NEW_PID), tx_bytes growing"
    SUCCESS=1
    break
  fi
done

if [ $SUCCESS -ne 1 ]; then
  # Cleanup: unfreeze the original PID so the rig isn't left wedged
  run_sudo "kill -CONT $PID" >/dev/null 2>&1 || true
  echo "FAIL: wfb_tx did not respawn within 70 s of SIGSTOP"
  echo "  last tx_bytes: $CURRENT (baseline $BEFORE, delta $((CURRENT - BEFORE)))"
  echo "  watchdog state on rig:"
  run_remote "echo '$PASS' | sudo -S journalctl -u ados-wfb -n 30 --since '2 minutes ago' 2>/dev/null | grep -iE 'zombie|tx_health|tx_byte' | tail -10"
  exit 2
fi

echo "=== test_wfb_tx_zombie_recovery PASSED ==="
exit 0
