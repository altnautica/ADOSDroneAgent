#!/usr/bin/env bash
# Boot `ados demo` and prove it actually came up.
#
# `ados demo` is the no-hardware path: it must start on a plain laptop, with no
# sudo, no SBC, no radio and no flight controller. Nothing in CI has ever booted
# it, so its health has only been asserted by reading the code.
#
# What the existing gates do and do not cover, measured rather than assumed by
# deleting a module the demo path imports at module level and re-running them:
# `ruff` passes (the symbol still resolves statically), and `pytest` catches it
# through exactly one test, which drives the CLI with the app mocked out. That
# test proves the entry point still imports; it cannot prove the process reaches
# a serving state, that every registered service survived startup, or that it
# shuts down. Those are what this covers.
#
# What this asserts, in order of how much it proves:
#
#   1. The process starts and stays up.
#   2. It logs `agent_started` -- so service registration finished, not just
#      "the interpreter did not exit yet".
#   3. No service reported a failure and no traceback was printed. A service
#      that dies still leaves HTTP serving, so a status code alone would miss it.
#   4. The REST API answers with a real config document, not merely a 200.
#   5. No service dies in the settle window after startup (see SETTLE_SECONDS).
#   6. SIGTERM shuts it down cleanly and promptly.
#
# macOS is NOT covered by a CI leg, deliberately: a macOS runner is billed at a
# multiple of a Linux one, and this is a cheap check to run by hand. Run it on
# the Mac (or the Linux VM) before shipping anything that touches the demo path.
# The two platforms differ in ways this script cannot paper over -- board
# detection, the runtime-dir base, and the mDNS stack all branch -- so a green
# Linux leg is not evidence about macOS.
#
# Usage:  scripts/smoke-demo.sh [port]
# Exits 0 on success; on failure prints the captured log and exits non-zero.

set -uo pipefail

PORT="${1:-18099}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
LOG="${WORK}/demo.log"
PID=""

# Keep the demo off every real path base. There are three independent ones and
# overriding only the run dir leaves the Linux run reaching /etc/ados and
# /var/ados, where it passes only because the two startup writes there happen to
# swallow PermissionError. That is luck, not isolation, and it also made the
# Linux and macOS runs exercise different trees -- on macOS all three already
# fall back under HOME.
export ADOS_RUN_DIR="${WORK}/run"
export ADOS_ETC_DIR="${WORK}/etc"
export ADOS_VAR_DIR="${WORK}/var"
export HOME="${WORK}/home"
mkdir -p "$ADOS_RUN_DIR" "$ADOS_ETC_DIR" "$ADOS_VAR_DIR" "$HOME"

cleanup() {
    if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
        # The group, not just the parent: `ados demo` spawns the MAVLink router
        # as a child when one is installed, and killing only the parent leaves it
        # running. Harmless on a CI runner where no router exists; on a dev
        # machine it orphans a live process on every failed run.
        kill -9 -- "-$PID" 2>/dev/null || kill -9 "$PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "::error::smoke-demo: $*"
    echo "--- captured demo log ---"
    [ -f "$LOG" ] && cat "$LOG"
    echo "--- end log ---"
    exit 1
}

command -v ados >/dev/null 2>&1 || fail "the 'ados' CLI is not on PATH; install the package first"
# Guarded so a missing tool reports itself. Without this the readiness loop
# just times out and blames the agent for not answering.
command -v curl >/dev/null 2>&1 || fail "curl is required by this script but is not on PATH"
command -v python3 >/dev/null 2>&1 || fail "python3 is required by this script but is not on PATH"

echo "smoke-demo: starting 'ados demo --port ${PORT}'"
cd "$REPO_ROOT" || fail "could not enter the repo root ${REPO_ROOT}"
# Job control on, so the background job becomes its own process-group leader and
# cleanup can kill the group. Without this `&` leaves the child in the script's
# own group, `kill -- -$PID` has no group to name, and the "kill the group" in
# cleanup would quietly fall back to killing only the parent.
set -m
ados demo --port "$PORT" >"$LOG" 2>&1 &
PID=$!

# 1. Wait for the REST API to answer, bounded. A dead process short-circuits the
#    wait so a startup crash reports in a second rather than after the timeout.
ready=""
for _ in $(seq 1 40); do
    if ! kill -0 "$PID" 2>/dev/null; then
        fail "the demo process exited during startup"
    fi
    code="$(curl -sS -m 2 -o "${WORK}/body.json" -w '%{http_code}' \
        "http://127.0.0.1:${PORT}/api/config" 2>/dev/null || true)"
    if [ "$code" = "200" ]; then
        ready=1
        break
    fi
    sleep 1
done
[ -n "$ready" ] || fail "the REST API never answered on port ${PORT} (40 attempts, up to ~2 min against a hung server)"

# 2. Service registration finished. Without this, a process that bound the port
#    and then stalled would still pass.
#
#    Polled, not checked once: the REST API starts serving before registration
#    finishes (mDNS registration sits between them), so a single check here races
#    and fails on a perfectly healthy start.
started=""
for _ in $(seq 1 30); do
    if grep -q "agent_started" "$LOG"; then
        started=1
        break
    fi
    if ! kill -0 "$PID" 2>/dev/null; then
        fail "the demo process exited before finishing service registration"
    fi
    sleep 1
done
[ -n "$started" ] || fail "the API answered but 'agent_started' was never logged within 30s"

# 3. No service failed and nothing raised. register_services catches per-service
#    exceptions and marks the service FAILED, so the process survives a broken
#    service and would otherwise look healthy here.
if grep -qE "service_failed|Traceback \(most recent call last\)" "$LOG"; then
    fail "a service failed to start (see the log below)"
fi

# 4. The body is a real config document. A 200 with an error page, an empty body
#    or an HTML redirect would pass a status-code check.
python3 - "$WORK/body.json" <<'PY' || fail "/api/config did not return a usable config document"
import json, sys

with open(sys.argv[1]) as fh:
    doc = json.load(fh)

if not isinstance(doc, dict):
    raise SystemExit(f"expected a JSON object, got {type(doc).__name__}")

# `agent` and `api` are the two sections every profile carries; requiring them
# means an empty `{}` (which is valid JSON) does not pass.
missing = [k for k in ("agent", "api") if k not in doc]
if missing:
    raise SystemExit(f"config document is missing {missing}")
PY

# 5. Watch a little longer for a service that dies AFTER registration finished.
#    Step 3 runs the instant `agent_started` appears, and the failures a deletion
#    causes often surface when a loop first touches the missing thing rather than
#    at import -- so the interesting death is usually a second or two later, not
#    at startup.
#
#    The window is bounded and stated rather than implied: SETTLE_SECONDS after
#    registration. A boot check cannot speak for a service that dies minutes in;
#    that is what the runtime service tracker is for. Measured: without the
#    dwell, a canary raising 6s after startup passed this script cleanly, because
#    everything from readiness to SIGTERM completes in about three seconds.
SETTLE_SECONDS=8
for _ in $(seq 1 "$SETTLE_SECONDS"); do
    if grep -qE "service_failed|Traceback \(most recent call last\)" "$LOG"; then
        fail "a service failed after startup completed (see the log below)"
    fi
    if ! kill -0 "$PID" 2>/dev/null; then
        fail "the demo process exited on its own after startup completed"
    fi
    sleep 1
done

# 6. Clean, prompt shutdown on SIGTERM -- the signal systemd and CI both send.
kill -TERM "$PID" 2>/dev/null || fail "could not signal the demo process"
stopped=""
for _ in $(seq 1 20); do
    if ! kill -0 "$PID" 2>/dev/null; then
        stopped=1
        break
    fi
    sleep 1
done
if [ -z "$stopped" ]; then
    fail "the demo process did not exit within 20s of SIGTERM"
fi
grep -q "agent_stopped" "$LOG" || fail "the process exited without logging a clean stop"

PID=""
echo "smoke-demo: ok -- demo started, served /api/config, and stopped cleanly"
