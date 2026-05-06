#!/usr/bin/env bash
# Memory soak harness for the lite ADOS Drone Agent on Luckfox.
# Run on the device: `bash luckfox-mem-profile.sh 300` for a 5-minute
# soak. Samples /proc/<pid>/status VmRSS once per second across the
# agent + rkmpi-wrapper + wfb_tx processes. Fails (exit 1) if peak
# combined RSS crosses the budget for the device's RAM tier.

set -eu
DURATION="${1:-300}"  # seconds
RAM_BUDGET_MB="${ADOS_RAM_BUDGET_MB:-220}"  # 256 MB device, 36 MB headroom
SAMPLE_INTERVAL="${ADOS_MEM_SAMPLE_INTERVAL:-1}"  # seconds

processes=("ados-agent-lite" "rkmpi-wrapper" "wfb_tx")

read_rss_kb() {
    local pid="$1"
    awk '/^VmRSS:/ { print $2 }' "/proc/$pid/status" 2>/dev/null || echo 0
}

pid_for() {
    pgrep -f "^[^ ]*$1" | head -n1 || true
}

peak_kb=0
samples=0
start=$(date +%s)
end=$((start + DURATION))

while [ "$(date +%s)" -lt "$end" ]; do
    total=0
    for proc in "${processes[@]}"; do
        pid=$(pid_for "$proc")
        if [ -n "$pid" ]; then
            kb=$(read_rss_kb "$pid")
            total=$((total + kb))
        fi
    done
    if [ "$total" -gt "$peak_kb" ]; then
        peak_kb="$total"
    fi
    samples=$((samples + 1))
    sleep "$SAMPLE_INTERVAL"
done

peak_mb=$((peak_kb / 1024))
echo "Peak combined RSS over ${DURATION}s: ${peak_mb} MB (samples: ${samples})"
echo "Budget: ${RAM_BUDGET_MB} MB"

if [ "$peak_mb" -gt "$RAM_BUDGET_MB" ]; then
    echo "FAIL: peak RSS exceeds budget"
    exit 1
fi
echo "PASS"
