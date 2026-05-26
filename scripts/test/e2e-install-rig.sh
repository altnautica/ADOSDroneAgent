#!/usr/bin/env bash
# Many check functions below are dispatched by name through run_check (e.g.
# `run_check ... check_wfb_module_loaded`), and several helpers are called
# only from inside those checks, so shellcheck cannot see the indirect call
# site and flags them as never-invoked. Silence that single class of false
# positive at file scope rather than peppering each function.
# shellcheck disable=SC2329
# =============================================================================
# e2e-install-rig.sh — hardware end-to-end install validation for the full
# Python ADOS Drone Agent.
#
# This runs against a REAL dev rig (an SBC with a real kernel, real RTL8812EU
# adapter, real board fingerprint). It validates exactly the things a
# container or emulator cannot: that the out-of-tree 8812eu kernel module
# actually loads on the running kernel, that the WFB radio interface
# enumerates, that the board auto-detect resolves to a known profile, and
# that the install success contract on disk matches reality.
#
# It drives a fresh install end to end, then asserts each piece of the
# shipped install contract as a discrete check, collecting failures so one
# miss does not abort the rest, and exits non-zero only if a REQUIRED check
# failed.
#
# There is intentionally NO Docker and NO QEMU path here. Run it against the
# bench rigs at validation time:
#
#   drone on the air-side rig:
#     scripts/test/e2e-install-rig.sh \
#       --host skynode.local --user radxa --profile drone
#
#   ground station on the ground-side rig:
#     scripts/test/e2e-install-rig.sh \
#       --host groundnode.local --user skynode --profile ground-station
#
# When --host is omitted the script runs locally on the rig it is invoked on.
#
# Auth (SSH): the script uses ssh and never embeds a password. Provide a key
# (preferred), or export ADOS_RIG_PASS and have sshpass on PATH for
# password auth on a bench box.
#
# Usage:
#   e2e-install-rig.sh [--host HOST] [--user USER]
#                      [--profile drone|ground-station]
#                      [--channel edge|stable]
#
# The full agent's production install runs inline (foreground), streaming its
# full output to the operator's terminal. The e2e run drives it the same way:
# it observes install completion and the install exit code synchronously, in
# this process, before it asserts the contract.
# =============================================================================

set -uo pipefail

# ─── Defaults ────────────────────────────────────────────────────────────────
RIG_HOST=""
RIG_USER=""
PROFILE="drone"
CHANNEL="edge"

# The agent REST surface and on-disk contract paths the install ships.
API_BASE="http://localhost:8080"
PAIRING_INFO_PATH="/api/pairing/info"
INSTALL_RESULT="/var/lib/ados/install-result.json"
CHECKPOINT_DIR="/var/lib/ados/install-checkpoints"
SUPERVISOR_UNIT="ados-supervisor"
WFB_MODULE="8812eu"

# Canonical install raw-script URL (curl-pipe path). When running locally on
# a checkout the sibling install.sh is used directly instead.
INSTALL_RAW_URL="https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh"

# Resolve our own location so a local run can find the sibling install.sh.
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
REPO_ROOT="$(cd "${SELF_DIR}/../.." && pwd)"

# ─── Result accumulators ─────────────────────────────────────────────────────
# Each check appends one line "STATUS|severity|name|detail" to RESULTS so the
# final summary can print every ✓/✗ and decide the exit code. STATUS is
# pass | fail | warn. severity is required | optional.
RESULTS=()
REQUIRED_FAILS=0

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

# ─── Logging ─────────────────────────────────────────────────────────────────
log()  { printf '%b\n' "${BLUE}[e2e]${NC} $*"; }
err()  { printf '%b\n' "${RED}[e2e]${NC} $*" >&2; }

usage() {
    cat <<'EOF'
e2e-install-rig.sh — hardware end-to-end install validation (full Python agent)

Usage:
  e2e-install-rig.sh [--host HOST] [--user USER]
                     [--profile drone|ground-station]
                     [--channel edge|stable]
                     [-h|--help]

Options:
  --host HOST        SSH target rig (e.g. skynode.local). Omit to run locally.
  --user USER        SSH user (defaults to the current user when --host set).
  --profile P        drone (default) or ground-station. Drives expected units.
  --channel C        edge (default) or stable. Sets ADOS_RELEASE_CHANNEL.
  -h, --help         Show this help and exit.

Auth (when --host is set):
  Uses ssh with a key by default. For a password bench box export ADOS_RIG_PASS
  and have sshpass on PATH. The script never embeds a password.

Examples:
  # drone, air-side rig
  e2e-install-rig.sh --host skynode.local --user radxa --profile drone

  # ground station, ground-side rig
  e2e-install-rig.sh --host groundnode.local --user skynode \
    --profile ground-station
EOF
}

# ─── Arg parsing ─────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --host)       RIG_HOST="${2:-}"; shift 2 ;;
        --user)       RIG_USER="${2:-}"; shift 2 ;;
        --profile)    PROFILE="${2:-}"; shift 2 ;;
        --channel)    CHANNEL="${2:-}"; shift 2 ;;
        -h|--help)    usage; exit 0 ;;
        *)            err "Unknown option: $1"; usage; exit 2 ;;
    esac
done

# Normalize the profile to the two accepted forms; map the unit-naming
# variant (underscore) used by the installer's expected_profile_units to the
# CLI flag form (hyphen). We carry both: PROFILE_FLAG for install.sh, and
# PROFILE_UNITS to pick the expected systemd unit set.
case "${PROFILE}" in
    drone)
        PROFILE_FLAG="drone"
        ;;
    ground-station|ground_station)
        PROFILE_FLAG="ground-station"
        ;;
    *)
        err "Invalid --profile '${PROFILE}' (expected drone|ground-station)"
        exit 2
        ;;
esac

case "${CHANNEL}" in
    edge|stable) : ;;
    *) err "Invalid --channel '${CHANNEL}' (expected edge|stable)"; exit 2 ;;
esac

# ─── Remote/local command helper ─────────────────────────────────────────────
# rsh CMD... — run CMD on the rig (over SSH when --host set, else locally).
# Honors --host/--user. Uses sshpass + ADOS_RIG_PASS only when the env var
# is present; otherwise plain ssh (key auth). Never embeds a password.
#
# The command string is passed to a remote bash -c so pipelines, redirects,
# and globbing behave the same locally and remotely. Quote args at the
# call site as you would for `bash -c`.
SSH_OPTS=(-o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new -o BatchMode=no)
rsh() {
    local cmd="$*"
    if [ -z "${RIG_HOST}" ]; then
        bash -c "${cmd}"
        return $?
    fi
    local target="${RIG_HOST}"
    [ -n "${RIG_USER}" ] && target="${RIG_USER}@${RIG_HOST}"
    # The command string is intentionally expanded on the rig (the remote
    # shell), not the client — that is the whole point of running checks
    # against the rig's environment, so the SC2029 client-side note is moot.
    if [ -n "${ADOS_RIG_PASS:-}" ] && command -v sshpass >/dev/null 2>&1; then
        # shellcheck disable=SC2029
        sshpass -p "${ADOS_RIG_PASS}" ssh "${SSH_OPTS[@]}" "${target}" "${cmd}"
    else
        # shellcheck disable=SC2029
        ssh "${SSH_OPTS[@]}" "${target}" "${cmd}"
    fi
}

# rsh_root CMD... — same as rsh but runs CMD as root on the rig. Uses sudo,
# feeding ADOS_RIG_PASS over stdin when present (sudo -S) so a passworded
# bench account works without a tty. When ADOS_RIG_PASS is unset, assumes
# passwordless sudo (the rig is configured for it, or you are already root).
rsh_root() {
    local cmd="$*"
    if [ -n "${ADOS_RIG_PASS:-}" ]; then
        # -S reads the password from stdin; -p '' silences the prompt so it
        # does not pollute captured output.
        rsh "echo '${ADOS_RIG_PASS}' | sudo -S -p '' bash -c $(_shquote "${cmd}")"
    else
        rsh "sudo bash -c $(_shquote "${cmd}")"
    fi
}

# _shquote STR — single-quote STR for safe embedding in a bash -c argument.
# Wraps STR in single quotes and rewrites every embedded single quote as the
# canonical POSIX sequence '\'' so the whole string survives one more layer
# of shell parsing intact.
_shquote() {
    local s="$1"
    local q="'\\''"
    printf "'%s'" "${s//\'/${q}}"
}

# ─── Check recorder ──────────────────────────────────────────────────────────
# record STATUS SEVERITY NAME DETAIL — append a result line and tally
# required failures. STATUS in pass|fail|warn; SEVERITY in required|optional.
record() {
    local status="$1" severity="$2" name="$3" detail="${4:-}"
    RESULTS+=("${status}|${severity}|${name}|${detail}")
    if [ "${status}" = "fail" ] && [ "${severity}" = "required" ]; then
        REQUIRED_FAILS=$((REQUIRED_FAILS + 1))
    fi
    case "${status}" in
        pass) printf '%b\n' "  ${GREEN}✓${NC} ${name}${detail:+  (${detail})}" ;;
        fail) printf '%b\n' "  ${RED}✗${NC} ${name}${detail:+  (${detail})}" ;;
        warn) printf '%b\n' "  ${YELLOW}!${NC} ${name}${detail:+  (${detail})}" ;;
    esac
}

# run_check NAME SEVERITY FUNC — run FUNC in a guarded subshell-free manner so
# a non-zero return or set -u trip inside it never aborts the whole run. FUNC
# is expected to call record() itself; run_check only shields the caller. We
# do NOT use `set -e` in this script precisely so a single failing check is
# isolated, but we still trap unexpected aborts defensively.
run_check() {
    local name="$1" severity="$2" func="$3"
    if ! "${func}"; then
        # FUNC already recorded its own result in the normal case; this guard
        # only fires when FUNC itself errored out before recording. Record a
        # generic failure so the summary still reflects it.
        if ! _last_recorded_is "${name}"; then
            record fail "${severity}" "${name}" "check errored"
        fi
    fi
    return 0
}

# _last_recorded_is NAME — true when the most recent RESULTS entry is for NAME.
_last_recorded_is() {
    local want="$1"
    local last="${RESULTS[${#RESULTS[@]}-1]:-}"
    [ -n "${last}" ] && [ "${last#*|*|"${want}"|}" != "${last}" ]
}

# ─── HTTP helper (runs ON the rig so localhost:8080 is the agent) ────────────
# api_get PATH — curl the agent REST API on the rig, print body to stdout,
# return curl's exit code. Short timeout; the retry loop lives in the check.
api_get() {
    local path="$1"
    rsh "curl -fsS --max-time 4 '${API_BASE}${path}' 2>/dev/null"
}

# ─── JSON field reader (python3 on the rig) ──────────────────────────────────
# json_field FILE_OR_STDIN KEY — print a top-level string/number field from a
# JSON document on the rig. When FILE is "-", reads the JSON from the second
# positional arg passed via stdin emulation is avoided; callers pass a file
# path that exists on the rig. Uses python3 which the install guarantees.
rig_json_file_field() {
    local file="$1" key="$2"
    rsh "python3 - '${file}' '${key}' <<'PY' 2>/dev/null
import json, sys
try:
    with open(sys.argv[1]) as fh:
        d = json.load(fh)
except Exception:
    sys.exit(1)
v = d.get(sys.argv[2])
if v is None:
    sys.exit(1)
print(v)
PY"
}

# rig_json_str_field KEY  (reads JSON piped on the rig) — used for the API
# body which we already have in a shell var. We re-fetch on the rig and parse
# there so we never depend on python3 on THIS macOS/dev host.
rig_parse_api_field() {
    local path="$1" key="$2"
    rsh "curl -fsS --max-time 4 '${API_BASE}${path}' 2>/dev/null | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
v = d.get(sys.argv[1])
if v is None:
    sys.exit(1)
print(v)
' '${key}' 2>/dev/null"
}

# =============================================================================
# Stage one — drive the install
# =============================================================================
INSTALL_EXIT=-1

drive_install() {
    log "Driving full-agent install: profile=${PROFILE_FLAG} channel=${CHANNEL}"

    # Compose the env the install body reads. ADOS_RELEASE_CHANNEL routes the
    # stable-vs-edge artifact selection. The install runs inline, so we
    # observe its exit code synchronously in this process.
    local env_prefix="ADOS_RELEASE_CHANNEL='${CHANNEL}'"

    local install_cmd
    if [ -z "${RIG_HOST}" ] && [ -f "${REPO_ROOT}/scripts/install.sh" ]; then
        # Local run on a checkout: invoke the sibling install.sh directly so
        # we test the exact tree under review (no network round-trip to main).
        log "Local checkout detected; running ${REPO_ROOT}/scripts/install.sh"
        install_cmd="${env_prefix} bash '${REPO_ROOT}/scripts/install.sh' --profile '${PROFILE_FLAG}'"
    else
        # Remote rig or no local checkout: run the canonical curl-pipe
        # one-liner so we validate the operator's real bootstrap path.
        log "Running curl-pipe install from ${INSTALL_RAW_URL}"
        install_cmd="${env_prefix} bash -c \"curl -fsSL '${INSTALL_RAW_URL}' | bash -s -- --profile '${PROFILE_FLAG}'\""
    fi

    # Run as root, synchronously, and capture the exit code. The install can
    # take many minutes (DKMS compile on the fallback path), so keep the SSH
    # session open for the full inline run.
    rsh_root "${install_cmd}"
    INSTALL_EXIT=$?
    log "Install process exited with code ${INSTALL_EXIT}"
}

check_install_exit() {
    if [ "${INSTALL_EXIT}" -eq 0 ]; then
        record pass required "install exit code == 0"
    else
        record fail required "install exit code == 0" "got ${INSTALL_EXIT}"
    fi
}

# =============================================================================
# Stage two — assert the contract
# =============================================================================

check_ados_version() {
    # The shipped CLI surfaces the version through `ados status --json` (key
    # "version"); `ados version` and `ados --version` are tried first for
    # forward-compat. Any path that yields a non-empty version with exit 0
    # passes. We resolve to whichever the installed CLI honors.
    local v=""
    v="$(rsh "ados version 2>/dev/null" || true)"
    if [ -z "${v}" ]; then
        v="$(rsh "ados --version 2>/dev/null" || true)"
    fi
    if [ -z "${v}" ]; then
        v="$(rsh "ados status --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get(\"version\",\"\"))' 2>/dev/null" || true)"
    fi
    v="$(printf '%s' "${v}" | tr -d '[:space:]')"
    if [ -n "${v}" ] && [ "${v}" != "?" ]; then
        record pass required "ados CLI reports a version" "${v}"
    else
        record fail required "ados CLI reports a version" "no version from ados/status"
    fi
}

check_supervisor_active() {
    local state
    state="$(rsh "systemctl is-active ${SUPERVISOR_UNIT} 2>/dev/null" || true)"
    state="$(printf '%s' "${state}" | tr -d '[:space:]')"
    if [ "${state}" = "active" ]; then
        record pass required "${SUPERVISOR_UNIT} is active"
    else
        record fail required "${SUPERVISOR_UNIT} is active" "is-active=${state:-unknown}"
    fi
}

# expected_units — echo the systemd units that must be enabled for the
# profile under test. Mirrors expected_profile_units() in install.d so the
# e2e checks the same contract the installer's health gate writes.
expected_units() {
    case "${PROFILE_FLAG}" in
        ground-station)
            printf '%s\n' \
                "ados-supervisor.service" \
                "ados-wfb-rx.service" \
                "ados-mediamtx-gs.service" \
                "ados-hostapd.service" \
                "ados-dnsmasq-gs.service" \
                "ados-setup-captive.service"
            ;;
        *)
            printf '%s\n' "ados-supervisor.service"
            ;;
    esac
}

check_profile_units_enabled() {
    local unit state missing=""
    while IFS= read -r unit; do
        [ -z "${unit}" ] && continue
        state="$(rsh "systemctl is-enabled ${unit} 2>/dev/null" || true)"
        state="$(printf '%s' "${state}" | tr -d '[:space:]')"
        case "${state}" in
            enabled|enabled-runtime|static|alias|indirect) : ;;
            *) missing="${missing} ${unit}(${state:-not-found})" ;;
        esac
    done < <(expected_units)
    if [ -z "${missing}" ]; then
        record pass required "profile units enabled (${PROFILE_FLAG})"
    else
        record fail required "profile units enabled (${PROFILE_FLAG})" "missing:${missing}"
    fi
}

check_api_reachable() {
    # Retry with linear backoff: the supervisor brings the API up shortly
    # after the install finishes, so a cold first probe can miss.
    local attempt body=""
    for attempt in 1 2 3 4 5 6; do
        body="$(api_get "${PAIRING_INFO_PATH}" || true)"
        if [ -n "${body}" ]; then
            break
        fi
        sleep $((attempt * 2))
    done
    if [ -n "${body}" ]; then
        record pass required "agent REST answers ${PAIRING_INFO_PATH}"
    else
        record fail required "agent REST answers ${PAIRING_INFO_PATH}" "no response after retries"
    fi
}

check_board_detected() {
    # The board the agent auto-detected. /api/pairing/info carries a top-level
    # "board" string (e.g. rock-5c-lite). A generic-* value means board
    # fingerprint did NOT resolve to a known profile, which on a known dev
    # rig is a regression. We do not hard-fail when the value is literally
    # "unknown" only because the API was unreachable — that is already caught
    # by check_api_reachable; here a "generic-" prefix is the failure.
    local board
    board="$(rig_parse_api_field "${PAIRING_INFO_PATH}" board || true)"
    board="$(printf '%s' "${board}" | tr -d '[:space:]')"
    if [ -z "${board}" ] || [ "${board}" = "unknown" ]; then
        # Fall back to the install-result board (device-tree model) so we can
        # still report something, but treat an empty API board as a soft warn
        # rather than masking a real generic-* detection regression.
        record warn optional "board auto-detected (not generic)" "API board empty; see install-result"
        return 0
    fi
    case "${board}" in
        generic-*)
            record fail required "board auto-detected (not generic)" "got ${board}" ;;
        *)
            record pass required "board auto-detected (not generic)" "${board}" ;;
    esac
}

check_wfb_module_loaded() {
    # The single most container-impossible assertion: the out-of-tree 8812eu
    # kernel module is actually loaded on the running kernel.
    local loaded
    loaded="$(rsh "lsmod 2>/dev/null | awk '{print \$1}' | grep -qx '${WFB_MODULE}' && echo yes || echo no" || true)"
    loaded="$(printf '%s' "${loaded}" | tr -d '[:space:]')"
    if [ "${loaded}" = "yes" ]; then
        record pass required "${WFB_MODULE} kernel module loaded"
    else
        record fail required "${WFB_MODULE} kernel module loaded" "not in lsmod"
    fi
}

check_wfb_interface_present() {
    # The WFB radio adapter is present: either an interface is in a
    # monitor-capable state, or the RTL adapter (Realtek vendor id 0bda)
    # enumerates on USB. Either is sufficient proof the radio is wired.
    local iw_ok="no" usb_ok="no"

    # iw lists wiphy capabilities; "monitor" appearing under supported
    # interface modes means a monitor-capable radio is present.
    iw_ok="$(rsh "iw list 2>/dev/null | grep -qiE '\\* monitor' && echo yes || echo no" || true)"
    iw_ok="$(printf '%s' "${iw_ok}" | tr -d '[:space:]')"

    # lsusb 0bda: is the Realtek vendor id the RTL8812EU enumerates under.
    usb_ok="$(rsh "lsusb 2>/dev/null | grep -qiE '0bda:' && echo yes || echo no" || true)"
    usb_ok="$(printf '%s' "${usb_ok}" | tr -d '[:space:]')"

    if [ "${iw_ok}" = "yes" ] || [ "${usb_ok}" = "yes" ]; then
        local how=""
        [ "${iw_ok}" = "yes" ] && how="monitor-capable iface"
        [ "${usb_ok}" = "yes" ] && how="${how:+${how}, }RTL 0bda: enumerated"
        record pass required "WFB radio interface present" "${how}"
    else
        record fail required "WFB radio interface present" "no monitor iface, no 0bda: on USB"
    fi
}

check_install_result_file() {
    # The machine-readable install contract must exist with status ok, or
    # degraded with NO required failures (optional steps like the radio
    # driver are allowed to be degraded without flunking the install).
    local exists
    exists="$(rsh "test -f '${INSTALL_RESULT}' && echo yes || echo no" || true)"
    exists="$(printf '%s' "${exists}" | tr -d '[:space:]')"
    if [ "${exists}" != "yes" ]; then
        record fail required "${INSTALL_RESULT} exists" "file missing"
        return 0
    fi

    local status reqfail_count wfb_src
    status="$(rig_json_file_field "${INSTALL_RESULT}" status || true)"
    status="$(printf '%s' "${status}" | tr -d '[:space:]')"

    # requiredFailures is an array; count its elements on the rig.
    reqfail_count="$(rsh "python3 - '${INSTALL_RESULT}' <<'PY' 2>/dev/null
import json, sys
try:
    with open(sys.argv[1]) as fh:
        d = json.load(fh)
except Exception:
    print('ERR'); sys.exit(0)
print(len(d.get('requiredFailures') or []))
PY" || true)"
    reqfail_count="$(printf '%s' "${reqfail_count}" | tr -d '[:space:]')"

    if [ "${status}" = "ok" ]; then
        record pass required "install-result status ok/clean" "status=ok"
    elif [ "${status}" = "degraded" ] && [ "${reqfail_count}" = "0" ]; then
        record pass required "install-result status ok/clean" "status=degraded, 0 required failures"
    else
        record fail required "install-result status ok/clean" \
            "status=${status:-unknown}, requiredFailures=${reqfail_count:-?}"
    fi

    # The Wi-Fi driver is always compiled on-device via DKMS, so
    # wfbModuleSource should read "dkms" on a rig that built it. Empty is
    # allowed only when there is genuinely no radio adapter; treat any other
    # value as a soft warn so we surface it without flunking the whole run on
    # a contract-annotation nit.
    wfb_src="$(rig_json_file_field "${INSTALL_RESULT}" wfbModuleSource || true)"
    wfb_src="$(printf '%s' "${wfb_src}" | tr -d '[:space:]')"
    case "${wfb_src}" in
        dkms)
            record pass optional "wfbModuleSource is dkms" "${wfb_src}" ;;
        "")
            record warn optional "wfbModuleSource is dkms" "empty (no driver sentinel)" ;;
        *)
            record warn optional "wfbModuleSource is dkms" "unexpected '${wfb_src}'" ;;
    esac
}

check_required_checkpoints() {
    # Every REQUIRED step must have left a checkpoint marker. radio-driver is
    # OPTIONAL (a rig with no RTL adapter installs fine), so it is checked as
    # a warn-only.
    local required_cps=(deps venv agent-package systemd global-symlinks)
    local cp missing=""
    for cp in "${required_cps[@]}"; do
        if ! rsh "test -f '${CHECKPOINT_DIR}/${cp}.done'" >/dev/null 2>&1; then
            missing="${missing} ${cp}"
        fi
    done
    if [ -z "${missing}" ]; then
        record pass required "required install checkpoints present" "${required_cps[*]}"
    else
        record fail required "required install checkpoints present" "missing:${missing}"
    fi

    # radio-driver checkpoint — optional. Warn (don't fail) when absent.
    if rsh "test -f '${CHECKPOINT_DIR}/radio-driver.done'" >/dev/null 2>&1; then
        record pass optional "radio-driver checkpoint present"
    else
        record warn optional "radio-driver checkpoint present" "absent (no radio or driver miss)"
    fi
}

# camera + FC are OPTIONAL on a bench rig: warn, never fail.
check_optional_peripherals() {
    # FC connection state via the agent status surface. Best-effort.
    local fc_state cam_present
    fc_state="$(rsh "ados status --json 2>/dev/null | python3 -c '
import json,sys
try:
    d=json.load(sys.stdin)
except Exception:
    sys.exit(0)
fc=d.get(\"fc\") or d.get(\"flight_controller\") or {}
print(fc.get(\"connected\") if isinstance(fc, dict) else fc)
' 2>/dev/null" || true)"
    fc_state="$(printf '%s' "${fc_state}" | tr -d '[:space:]')"
    if [ "${fc_state}" = "True" ] || [ "${fc_state}" = "true" ]; then
        record pass optional "flight controller connected" "optional"
    else
        record warn optional "flight controller connected" "not connected (optional on bench)"
    fi

    # Camera presence via a UVC video node. Best-effort, optional.
    cam_present="$(rsh "ls /dev/video* >/dev/null 2>&1 && echo yes || echo no" || true)"
    cam_present="$(printf '%s' "${cam_present}" | tr -d '[:space:]')"
    if [ "${cam_present}" = "yes" ]; then
        record pass optional "camera video node present" "optional"
    else
        record warn optional "camera video node present" "no /dev/video* (optional on bench)"
    fi
}

# =============================================================================
# Banner + driver
# =============================================================================
print_banner() {
    printf '%b\n' "${BOLD}${BLUE}"
    printf '%s\n' "============================================================"
    printf '%s\n' " ADOS Drone Agent — hardware end-to-end install validation"
    printf '%s\n' "============================================================"
    printf '%b\n' "${NC}"
    printf '  %-12s %s\n' "Target:"  "${RIG_HOST:-<local rig>}${RIG_USER:+ (user ${RIG_USER})}"
    printf '  %-12s %s\n' "Profile:" "${PROFILE_FLAG}"
    printf '  %-12s %s\n' "Channel:" "${CHANNEL}"
    printf '  %-12s %s\n' "Driver:"  "${PREBUILT}"
    if [ -n "${RIG_HOST}" ]; then
        if [ -n "${ADOS_RIG_PASS:-}" ]; then
            printf '  %-12s %s\n' "SSH auth:" "sshpass (ADOS_RIG_PASS set)"
        else
            printf '  %-12s %s\n' "SSH auth:" "key (BatchMode/agent)"
        fi
    fi
    printf '\n'
}

print_summary() {
    printf '\n%b\n' "${BOLD}Summary${NC}"
    printf '%s\n' "------------------------------------------------------------"
    local line status severity name detail
    local pass_n=0 fail_n=0 warn_n=0
    for line in "${RESULTS[@]}"; do
        IFS='|' read -r status severity name detail <<<"${line}"
        case "${status}" in
            pass) pass_n=$((pass_n+1));  printf '%b\n' "  ${GREEN}✓${NC} [${severity}] ${name}${detail:+  — ${detail}}" ;;
            fail) fail_n=$((fail_n+1));  printf '%b\n' "  ${RED}✗${NC} [${severity}] ${name}${detail:+  — ${detail}}" ;;
            warn) warn_n=$((warn_n+1));  printf '%b\n' "  ${YELLOW}!${NC} [${severity}] ${name}${detail:+  — ${detail}}" ;;
        esac
    done
    printf '%s\n' "------------------------------------------------------------"
    printf '  %bpass=%d%b  %bfail=%d%b  %bwarn=%d%b  (required failures: %d)\n' \
        "${GREEN}" "${pass_n}" "${NC}" \
        "${RED}" "${fail_n}" "${NC}" \
        "${YELLOW}" "${warn_n}" "${NC}" \
        "${REQUIRED_FAILS}"

    if [ "${REQUIRED_FAILS}" -gt 0 ]; then
        printf '\n%b\n' "${BOLD}${RED}RESULT: FAIL${NC} — ${REQUIRED_FAILS} required check(s) failed."
        return 1
    fi
    printf '\n%b\n' "${BOLD}${GREEN}RESULT: PASS${NC} — all required checks passed."
    return 0
}

main() {
    print_banner

    # A quick reachability probe so a typo'd host fails fast with a clear
    # message instead of timing out on every check.
    if [ -n "${RIG_HOST}" ]; then
        log "Probing SSH connectivity to ${RIG_HOST}..."
        if ! rsh "true" >/dev/null 2>&1; then
            err "Cannot reach the rig over SSH (host=${RIG_HOST} user=${RIG_USER:-current})."
            err "Check the hostname, your key/agent, or export ADOS_RIG_PASS with sshpass on PATH."
            exit 2
        fi
        log "SSH OK."
    fi

    # ── Stage one: install ──
    printf '\n%b\n' "${BOLD}Stage one — install${NC}"
    drive_install

    # ── Stage two: contract assertions ──
    printf '\n%b\n' "${BOLD}Stage two — contract checks${NC}"
    run_check "install exit"        required check_install_exit
    run_check "ados version"        required check_ados_version
    run_check "supervisor active"   required check_supervisor_active
    run_check "profile units"       required check_profile_units_enabled
    run_check "api reachable"       required check_api_reachable
    run_check "board detected"      required check_board_detected
    run_check "wfb module"          required check_wfb_module_loaded
    run_check "wfb interface"       required check_wfb_interface_present
    run_check "install-result"      required check_install_result_file
    run_check "checkpoints"         required check_required_checkpoints
    run_check "optional peripherals" optional check_optional_peripherals

    # ── Summary + exit ──
    if print_summary; then
        exit 0
    else
        exit 1
    fi
}

main "$@"
