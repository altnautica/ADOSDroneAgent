#!/usr/bin/env bash
# test_wfb_rx_zombie_recovery.sh — exercise the GS-side RX-liveness watchdog.
#
# Symmetric to test_wfb_tx_zombie_recovery.sh. SIGSTOP wfb_rx, then
# poll /sys/class/net/<iface>/statistics/rx_packets and assert the
# watchdog respawns the process within 60-70 s.
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
IFACE=$(run_remote 'ls /sys/class/net | grep -E "^wlan|^wlx" | head -1' | tr -d '\r\n')
if [ -z "$IFACE" ]; then
  echo "FAIL: no wlan iface found"
  exit 1
fi
echo "iface=$IFACE"

PID=$(run_remote 'pgrep -f "^wfb_rx -p 0" | head -1' | tr -d '\r\n')
if [ -z "$PID" ]; then
  echo "FAIL: wfb_rx not running (start ados-wfb-rx first)"
  exit 1
fi
echo "wfb_rx pid=$PID"

BEFORE=$(run_remote "cat /sys/class/net/$IFACE/statistics/rx_packets" | tr -d '\r\n')
echo "rx_packets baseline=$BEFORE"

echo "freezing wfb_rx with SIGSTOP..."
run_sudo "kill -STOP $PID" >/dev/null

echo "waiting up to 70 s for watchdog + respawn..."
SUCCESS=0
for i in $(seq 1 14); do
  sleep 5
  CURRENT=$(run_remote "cat /sys/class/net/$IFACE/statistics/rx_packets" | tr -d '\r\n')
  NEW_PID=$(run_remote 'pgrep -f "^wfb_rx -p 0" | head -1' | tr -d '\r\n')
  echo "  t=$((i*5))s rx_packets=$CURRENT pid=$NEW_PID"
  if [ -n "$NEW_PID" ] && [ "$NEW_PID" != "$PID" ] && [ "$CURRENT" -gt "$BEFORE" ]; then
    echo "PASS: watchdog respawned wfb_rx ($PID -> $NEW_PID), rx_packets growing"
    SUCCESS=1
    break
  fi
done

if [ $SUCCESS -ne 1 ]; then
  run_sudo "kill -CONT $PID" >/dev/null 2>&1 || true
  echo "FAIL: wfb_rx did not respawn within 70 s of SIGSTOP"
  echo "  last rx_packets: $CURRENT (baseline $BEFORE, delta $((CURRENT - BEFORE)))"
  echo "  watchdog state on rig:"
  run_remote "echo '$PASS' | sudo -S journalctl -u ados-wfb-rx -n 30 --since '2 minutes ago' 2>/dev/null | grep -iE 'zombie|rx_health|rx_pkt' | tail -10"
  exit 2
fi

echo "=== test_wfb_rx_zombie_recovery PASSED ==="
exit 0
