# shellcheck shell=bash
# =============================================================================
# 14-orchestration.sh — completeness probe, per-step checkpoints, and the
# install success contract + result file.
#
# Sourced last after 13-main.sh so its helpers are in scope before
# main_install_flow runs the health gate.
#
# Two concerns live here:
#
#   1. Completeness + checkpoints. is_install_complete answers "is every
#      REQUIRED component actually present and enabled", not just "is the
#      binary on disk". Per-step checkpoints under
#      /var/lib/ados/install-checkpoints let a resumed run skip finished
#      modules and let `ados install --status` show what is done vs missing.
#
#   2. Success contract. run_health_gate asserts the REQUIRED components are
#      live and writes /var/lib/ados/install-result.json with a machine
#      readable status. The dispatcher's exit code then reflects reality
#      instead of always returning 0.
# =============================================================================

# Long-lived state lives under /var/lib/ados (bootstrapped by
# setup_state_dirs in 01-state.sh). These are the two contract paths the
# heartbeat and the CLI read; keep the literals in one place.
export ADOS_STATE_DIR="${ADOS_STATE_DIR:-/var/lib/ados}"
export ADOS_CHECKPOINT_DIR="${ADOS_CHECKPOINT_DIR:-${ADOS_STATE_DIR}/install-checkpoints}"
export ADOS_INSTALL_RESULT="${ADOS_INSTALL_RESULT:-${ADOS_STATE_DIR}/install-result.json}"

# Source the shared network fetch helper (ados_fetch).
# install.d/lib.sh has already defined info/warn/error by the time this
# module is sourced, so net.sh's fallback loggers stay dormant. Resolve
# the path relative to this module so it works from a git clone, the
# curl-pipe bootstrap clone, and the persisted /opt/ados/source tree.
if ! declare -F ados_fetch >/dev/null 2>&1; then
    _ORCH_LIB_DIR=""
    if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
        _ORCH_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd)" || _ORCH_LIB_DIR=""
    fi
    if [ -n "${_ORCH_LIB_DIR}" ] && [ -f "${_ORCH_LIB_DIR}/net.sh" ]; then
        # shellcheck source=scripts/lib/net.sh disable=SC1091
        . "${_ORCH_LIB_DIR}/net.sh"
    elif [ -f /opt/ados/source/scripts/lib/net.sh ]; then
        # shellcheck disable=SC1091
        . /opt/ados/source/scripts/lib/net.sh
    fi
    unset _ORCH_LIB_DIR
fi

# ─── Bounded git clone retry ─────────────────────────────────────────────────
#
# git_clone_retry DEST [BRANCH] — clone REPO_URL into DEST with up to three
# attempts and linear backoff so a transient network blip on the upgrade /
# fresh-install clone does not fail the install. Returns the clone's exit
# status on the final attempt. The bootstrap clone in install.sh has its
# own inline copy of this loop because modules are not sourced yet on the
# curl-pipe path.
git_clone_retry() {
    local dest="$1" branch="${2:-}"
    local -a args=(clone --depth 1 --recurse-submodules --shallow-submodules --quiet)
    [ -n "${branch}" ] && args+=(--branch "${branch}")
    args+=("${REPO_URL}" "${dest}")
    local try
    for try in 1 2 3; do
        if git "${args[@]}"; then
            return 0
        fi
        warn "git clone attempt ${try} failed; retrying in $((try * 3))s..."
        rm -rf "${dest}" 2>/dev/null || true
        sleep $((try * 3))
    done
    return 1
}

# ─── Per-step checkpoints ────────────────────────────────────────────────────
#
# A checkpoint is an empty marker file at
# /var/lib/ados/install-checkpoints/<name>.done written when a REQUIRED
# step completes. A resumed run reads these to skip finished work; a
# half-install (interrupted before a step's marker landed) re-runs that
# step and every later one. The markers are advisory: every install step
# is independently idempotent, so a stale marker never corrupts state, it
# only changes how loudly the resume narrates.

checkpoint_dir() {
    install -d -m 0755 "${ADOS_CHECKPOINT_DIR}" 2>/dev/null || true
    printf '%s\n' "${ADOS_CHECKPOINT_DIR}"
}

# checkpoint_done NAME — return 0 if the named checkpoint marker exists.
checkpoint_done() {
    [ -f "${ADOS_CHECKPOINT_DIR}/$1.done" ]
}

# checkpoint_mark NAME — record that the named step finished. Idempotent.
checkpoint_mark() {
    checkpoint_dir >/dev/null
    : > "${ADOS_CHECKPOINT_DIR}/$1.done" 2>/dev/null || true
}

# checkpoint_clear — drop every checkpoint marker. Called on --force so a
# full reinstall does not trust stale markers from a prior partial run.
checkpoint_clear() {
    rm -f "${ADOS_CHECKPOINT_DIR}"/*.done 2>/dev/null || true
}

# list_completed_checkpoints — space-separated list of finished step names
# (marker basename minus .done), or "<none>" when nothing is recorded.
# Read-only; used by the resume narration and by `ados install --status`.
list_completed_checkpoints() {
    local out="" f name
    if [ -d "${ADOS_CHECKPOINT_DIR}" ]; then
        for f in "${ADOS_CHECKPOINT_DIR}"/*.done; do
            [ -f "${f}" ] || continue
            name="$(basename "${f}" .done)"
            out="${out} ${name}"
        done
    fi
    out="${out# }"
    printf '%s\n' "${out:-<none>}"
}

# ─── Profile unit expectations ───────────────────────────────────────────────
#
# The set of systemd units that MUST be enabled for a given profile to be
# considered a complete install. Drone needs only the supervisor (its
# child units are started dynamically by the supervisor from hardware
# detection and are not enable-linked individually). Ground-station needs
# the supervisor plus the receive + AP + setup units that
# enable_ground_station_units enable-links. Keep this list a strict subset
# of what that function enables so completeness never demands a unit the
# installer does not actually create.
expected_profile_units() {
    local profile="${1:-drone}"
    case "${profile}" in
        ground_station)
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

# unit_enabled UNIT — true when systemd reports the unit as enabled (or
# enabled-runtime / static / alias, all of which mean "will be brought up").
# Tolerates the not-found case so the probe never aborts under set -e.
unit_enabled() {
    local state
    state="$(systemctl is-enabled "$1" 2>/dev/null || true)"
    case "${state}" in
        enabled|enabled-runtime|static|alias|indirect) return 0 ;;
        *) return 1 ;;
    esac
}

# ─── Completeness probe ──────────────────────────────────────────────────────
#
# Replaces the shallow "the binary exists" gate. A complete install has:
#   - the global `ados` command on PATH (symlinked into /usr/local/bin)
#   - an importable venv (python can `import ados`)
#   - the supervisor unit installed AND enabled
#   - every profile-expected unit enabled
#
# Returns 0 only when ALL hold. On Linux the systemd checks are
# authoritative; on macOS dev mode there are no units, so completeness
# collapses to "command present + importable", matching dev-mode reality.
#
# Sets the global INSTALL_MISSING to a space-separated list of the failed
# checks so callers (and `ados install --status`) can report specifics.
INSTALL_MISSING=""
is_install_complete() {
    INSTALL_MISSING=""
    local ok=true

    # Global command. Prefer the symlink the installer lays down; fall
    # back to the venv binary so a dev install with PATH munging still
    # passes.
    if ! command -v ados >/dev/null 2>&1 && [ ! -x "${VENV_DIR}/bin/ados" ]; then
        INSTALL_MISSING="${INSTALL_MISSING} global-command"
        ok=false
    fi

    # Importable venv.
    if ! "${VENV_DIR}/bin/python" -c "import ados" >/dev/null 2>&1; then
        INSTALL_MISSING="${INSTALL_MISSING} venv-import"
        ok=false
    fi

    # systemd units (Linux only). macOS dev mode has no units.
    if [ "$(uname -s)" = "Linux" ]; then
        local profile="${1:-${ADOS_PROFILE:-drone}}"
        if [ ! -f "/etc/systemd/system/${SERVICE_NAME}.service" ]; then
            INSTALL_MISSING="${INSTALL_MISSING} supervisor-unit"
            ok=false
        fi
        local unit
        while IFS= read -r unit; do
            [ -z "${unit}" ] && continue
            if ! unit_enabled "${unit}"; then
                INSTALL_MISSING="${INSTALL_MISSING} unit:${unit}"
                ok=false
            fi
        done < <(expected_profile_units "${profile}")
    fi

    INSTALL_MISSING="${INSTALL_MISSING# }"
    [ "${ok}" = "true" ]
}

# ─── Install success contract + result file ──────────────────────────────────
#
# write_install_result STATUS — emit /var/lib/ados/install-result.json, the
# machine-readable contract a separate heartbeat change consumes. STATUS is
# one of ok | degraded | failed. The function reads the accumulated
# FAILED_STEPS / REQUIRED_FAILURES globals (space-separated) plus profile,
# board, kernel, and the wfb module source sentinel.
#
# JSON shape (stable contract — do not rename keys):
#   {
#     "status": "ok" | "degraded" | "failed",
#     "version": "<agent version or unknown>",
#     "profile": "drone" | "ground_station",
#     "board": "<board id or unknown>",
#     "kernelRelease": "<uname -r>",
#     "wfbModuleSource": "prebuilt" | "dkms" | "" ,
#     "failedSteps": ["<step>", ...],
#     "requiredFailures": ["<step>", ...],
#     "ts": "<UTC ISO8601>"
#   }
FAILED_STEPS=""
REQUIRED_FAILURES=""

# Set to 1 by write_install_result after it lands the result file. The
# install-body EXIT trap reads this so it only writes a fallback "failed"
# result when the normal path (run_health_gate -> write_install_result) never
# ran — i.e. the body aborted mid-step under set -e before reaching the gate.
# Exactly-once: whichever writer runs first flips the flag.
ADOS_RESULT_WRITTEN=0

# record_failure STEP REQUIRED — append a failed step to the result
# accumulators. REQUIRED=required marks it as a hard failure that flips the
# overall status to "failed" and forces a non-zero exit.
record_failure() {
    local step="$1" kind="${2:-optional}"
    FAILED_STEPS="${FAILED_STEPS} ${step}"
    if [ "${kind}" = "required" ]; then
        REQUIRED_FAILURES="${REQUIRED_FAILURES} ${step}"
    fi
}

write_install_result() {
    local status="$1"
    install -d -m 0755 "${ADOS_STATE_DIR}" 2>/dev/null || true

    local version="unknown"
    if declare -F get_installed_version >/dev/null 2>&1; then
        version="$(get_installed_version 2>/dev/null || echo unknown)"
    fi

    local profile="${ADOS_PROFILE:-drone}"
    local kernel; kernel="$(uname -r 2>/dev/null || echo unknown)"

    # Board id: read the agent's persisted board override if present, else
    # the device-tree model, else unknown. Cheap filesystem reads only.
    local board="unknown"
    if [ -r /etc/ados/board_override ]; then
        board="$(tr -d '[:space:]\000' < /etc/ados/board_override 2>/dev/null || echo unknown)"
    elif [ -r /proc/device-tree/model ]; then
        board="$(tr -d '\000' < /proc/device-tree/model 2>/dev/null || echo unknown)"
    fi
    [ -n "${board}" ] || board="unknown"

    # WFB kernel-module source sentinel dropped by the radio driver path
    # (prebuilt vs dkms). Absent on rigs with no RTL adapter / no driver.
    local wfb_src=""
    if [ -r /run/ados/wfb-module-source ]; then
        wfb_src="$(tr -d '[:space:]\000' < /run/ados/wfb-module-source 2>/dev/null || true)"
    fi

    local ts; ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    # Build the JSON with python3 (always present after deps) so arrays and
    # escaping are correct. Fall back to a hand-rolled writer if python3 is
    # somehow unavailable so the contract file always lands.
    if command -v python3 >/dev/null 2>&1; then
        ADOS_R_STATUS="${status}" ADOS_R_VERSION="${version}" \
        ADOS_R_PROFILE="${profile}" ADOS_R_BOARD="${board}" \
        ADOS_R_KERNEL="${kernel}" ADOS_R_WFB="${wfb_src}" \
        ADOS_R_FAILED="${FAILED_STEPS# }" ADOS_R_REQFAIL="${REQUIRED_FAILURES# }" \
        ADOS_R_TS="${ts}" ADOS_R_OUT="${ADOS_INSTALL_RESULT}" \
        python3 - <<'PY'
import json, os

def split(name):
    return [s for s in os.environ.get(name, "").split() if s]

result = {
    "status": os.environ["ADOS_R_STATUS"],
    "version": os.environ.get("ADOS_R_VERSION", "unknown"),
    "profile": os.environ.get("ADOS_R_PROFILE", "drone"),
    "board": os.environ.get("ADOS_R_BOARD", "unknown"),
    "kernelRelease": os.environ.get("ADOS_R_KERNEL", "unknown"),
    "wfbModuleSource": os.environ.get("ADOS_R_WFB", ""),
    "failedSteps": split("ADOS_R_FAILED"),
    "requiredFailures": split("ADOS_R_REQFAIL"),
    "ts": os.environ["ADOS_R_TS"],
}
out = os.environ["ADOS_R_OUT"]
tmp = out + ".tmp"
with open(tmp, "w", encoding="utf-8") as fh:
    json.dump(result, fh, indent=2)
    fh.write("\n")
os.replace(tmp, out)
PY
    else
        # Minimal fallback writer (no arrays beyond a flat join).
        {
            printf '{\n'
            printf '  "status": "%s",\n' "${status}"
            printf '  "version": "%s",\n' "${version}"
            printf '  "profile": "%s",\n' "${profile}"
            printf '  "board": "%s",\n' "${board}"
            printf '  "kernelRelease": "%s",\n' "${kernel}"
            printf '  "wfbModuleSource": "%s",\n' "${wfb_src}"
            printf '  "failedSteps": [],\n'
            printf '  "requiredFailures": [],\n'
            printf '  "ts": "%s"\n' "${ts}"
            printf '}\n'
        } > "${ADOS_INSTALL_RESULT}.tmp" && mv "${ADOS_INSTALL_RESULT}.tmp" "${ADOS_INSTALL_RESULT}"
    fi
    chmod 0644 "${ADOS_INSTALL_RESULT}" 2>/dev/null || true
    ADOS_RESULT_WRITTEN=1
}

# ─── Mid-step abort fallback (install-body EXIT trap) ──────────────────────────
#
# Under `set -e` a hard failure inside the install body (e.g. the venv pip
# crashing before ensure_venv_pip's reinstall, an apt step erroring out, a
# helper returning non-zero) kills the script before run_health_gate runs.
# Without a fallback no /var/lib/ados/install-result.json is written, so the
# heartbeat cannot report the failure and the operator sees a half-installed
# box with no machine-readable signal.
#
# install_failure_trap is wired as an EXIT trap by the dispatcher right before
# main_install_flow (so it never fires for the pair-only fast path, which exits
# cleanly without running the body). It writes a
# "failed" result only when:
#   - the script is exiting non-zero (an actual failure, not a clean exit 0), AND
#   - write_install_result has not already run (the health gate is the normal
#     path; this is strictly the fallback).
# The current step name (set by the body via ADOS_CURRENT_STEP) is recorded as
# a REQUIRED failure so the abort is attributable.
install_failure_trap() {
    local rc="$1"
    # Clean exit: the body finished and run_health_gate already wrote the
    # result (or this is a non-body exit). Nothing to do.
    [ "${rc}" -eq 0 ] && return 0
    # The normal path already wrote a result; don't clobber it.
    [ "${ADOS_RESULT_WRITTEN:-0}" = "1" ] && return 0
    # Non-Linux dev mode has no result-file contract.
    [ "$(uname -s)" = "Linux" ] || return 0

    local step="${ADOS_CURRENT_STEP:-install-aborted}"
    record_failure "${step}" required
    warn "Install aborted (exit ${rc}) at step '${step}' before the health gate; writing failed install result."
    write_install_result "failed"
}

# ─── Broken-venv-pip self-recovery ────────────────────────────────────────────
#
# A venv's pip can rot independently of the agent package: a partial system
# upgrade, an interrupted `pip install --upgrade pip`, or a Python minor-version
# bump under the venv can leave the bundled pip importing modules that no longer
# exist (e.g. a stale pip vendor tree raising ModuleNotFoundError on import).
# When that happens the next `pip install` on the upgrade path dies before it
# can touch the agent package, so the box silently stays on the old version.
#
# ensure_venv_pip probes the venv pip and self-heals when it is broken, in
# escalating order so the cheapest fix runs first:
#
#   probe   — `python -m pip --version`. Working pip => return 0, no-op.
#   stage 1 — `python -m ensurepip --upgrade` then bootstrap a fresh pip via
#             `python -m pip install --upgrade pip`. Re-probe.
#   stage 2 — pip still broken: recreate the whole venv from scratch using the
#             same `python -m venv --system-site-packages` flow the fresh
#             install uses, then re-run the caller-supplied reinstall callback
#             so the agent package lands in the rebuilt venv. Re-probe.
#
# REINSTALL_FN (optional) is a function name invoked after a venv recreate to
# reinstall the agent package into the rebuilt venv. The caller passes the
# channel-correct reinstaller (wheel on stable, source tree on edge). When pip
# cannot be recovered even after a recreate, the function records a REQUIRED
# failure (`venv-pip`) so the result file attributes the abort, and returns
# non-zero. A working or recovered pip returns 0.
#
# Safe to call on both the fresh and upgrade paths: on a healthy venv it is a
# single fast probe.

# ensure_build_toolchain: make the venv's Python build chain sound before any
# agent source build. Some recent pip releases (the 26.1.x line) crash with
# SIGSEGV the moment they spin up the isolated build-dependency environment on
# certain arm64 kernels, which kills the agent-software step outright. Pinning
# pip below that line restores correct PEP 517 build isolation for the whole
# dependency tree, including deps that build from sdist (pymavlink), so we keep
# normal isolation rather than --no-build-isolation: bypassing isolation would
# strip those sdist deps of their build environment and trade one breakage for
# another. We also stage setuptools>=68 (the agent's declared build backend)
# and wheel into the venv as a second layer. Every step is a plain WHEEL
# install, which never uses build isolation, so it succeeds even when the
# current pip's isolation is broken (this is what self-heals an already-broken
# pip on the rig). Idempotent and fast when already satisfied; best-effort
# (warn, not abort) so a blip on one leg does not sink the install.
ensure_build_toolchain() {
    # Clear corrupt leftover distributions first. pip renames a package to
    # ~name when an install or upgrade is interrupted (e.g. a killed build);
    # the half-written metadata then crashes the next pip run with a SIGSEGV
    # ("Ignoring invalid distribution ~name"). A glob delete clears them. The
    # tilde is mid-path here, so the shell does not tilde-expand it.
    rm -rf "${VENV_DIR}"/lib/python*/site-packages/~* 2>/dev/null || true
    # Pin pip away from the build-isolation regression. This is a wheel
    # install, so it works on the currently-broken pip too.
    "${VENV_DIR}/bin/python" -m pip install --upgrade 'pip>=24,<26' --quiet \
        || warn "Could not pin pip below 26; continuing with the current pip."
    # Stage the build backend the agent's pyproject declares. Wheel installs.
    "${VENV_DIR}/bin/python" -m pip install --upgrade 'setuptools>=68' wheel --quiet \
        || warn "Could not refresh setuptools/wheel; the source build may be degraded."
}

ensure_venv_pip() {
    local reinstall_fn="${1:-}"

    # Probe: a working venv pip needs no recovery.
    if "${VENV_DIR}/bin/python" -m pip --version >/dev/null 2>&1; then
        return 0
    fi

    warn "venv pip is broken (\`python -m pip --version\` failed). Attempting in-place repair."

    # Stage 1: re-bootstrap pip inside the existing venv via ensurepip, then
    # upgrade it. ensurepip ships pip's wheels with the interpreter, so this
    # recovers a corrupted/stale pip without a network round-trip for the
    # bootstrap itself.
    "${VENV_DIR}/bin/python" -m ensurepip --upgrade >/dev/null 2>&1 || true
    ensure_build_toolchain
    if "${VENV_DIR}/bin/python" -m pip --version >/dev/null 2>&1; then
        warn "venv pip repaired via ensurepip; continuing."
        return 0
    fi

    # Stage 2: the pip inside this venv is unrecoverable. Recreate the venv
    # from scratch with the same flags the fresh install uses, then reinstall
    # the agent package via the caller's channel-correct reinstaller.
    warn "ensurepip did not recover the venv pip; recreating the virtual environment."
    local py
    py="$(find_python)"
    if [ -z "${py}" ]; then
        error "No Python 3.11+ interpreter available to recreate the venv."
        record_failure "venv-pip" required
        return 1
    fi
    rm -rf "${VENV_DIR}"
    if ! "${py}" -m venv --system-site-packages "${VENV_DIR}"; then
        error "Recreating the venv failed."
        record_failure "venv-pip" required
        return 1
    fi
    # The freshly created venv ships a working pip; bring it current.
    ensure_build_toolchain

    if [ -n "${reinstall_fn}" ] && declare -F "${reinstall_fn}" >/dev/null 2>&1; then
        warn "Reinstalling the agent package into the rebuilt venv."
        "${reinstall_fn}" || warn "Agent-package reinstall into the rebuilt venv reported a non-zero status."
    fi

    if "${VENV_DIR}/bin/python" -m pip --version >/dev/null 2>&1; then
        warn "venv recreated with a working pip; continuing."
        return 0
    fi

    error "venv pip is still broken after recreate; the upgrade cannot proceed."
    record_failure "venv-pip" required
    return 1
}

# install_radio_driver_tracked — run the RTL8812EU driver installer and
# record an OPTIONAL failure when the kernel module did not land. The
# driver path is non-fatal by design (a rig with no RTL adapter installs
# fine), so this never blocks the install; it only annotates the result
# file's failedSteps so the heartbeat can flag a degraded radio. The
# driver writes /run/ados/wfb-module-source on success (prebuilt | dkms);
# its absence after the call means the module did not install this run.
# Also marks the optional `radio-driver` checkpoint on success.
install_radio_driver_tracked() {
    install_ground_station_driver || true
    if [ -r /run/ados/wfb-module-source ] || lsmod 2>/dev/null | grep -qE '^88(12|2c)[a-z]*u'; then
        checkpoint_mark radio-driver
        return 0
    fi
    warn "RTL8812EU kernel module not present after driver install; recording optional failure."
    record_failure "radio-driver" optional
    return 0
}

# run_health_gate — assert the REQUIRED components are actually up, write the
# result file, and return 0 only when no REQUIRED step failed. Called at the
# very end of the full-install + upgrade paths in 13-main.sh. Optional
# failures (radio driver, display overlay, OTG, mesh) downgrade status to
# "degraded" but still return 0; a REQUIRED failure returns non-zero so the
# dispatcher's exit code reflects the real outcome.
#
# REQUIRED components, in order of assertion:
#   - venv importable           (the agent package installed cleanly)
#   - supervisor unit active     (systemd is running the agent)
#   - profile units enabled      (the right child units are enable-linked)
#   - agent REST reachable       (the API answers on 127.0.0.1:8080)
run_health_gate() {
    [ "$(uname -s)" = "Linux" ] || { write_install_result "ok"; return 0; }

    local profile="${ADOS_PROFILE:-drone}"

    # 1. venv importable — REQUIRED.
    if ! "${VENV_DIR}/bin/python" -c "import ados" >/dev/null 2>&1; then
        record_failure "venv-import" required
    fi

    # 2. supervisor active — REQUIRED.
    if ! systemctl is-active --quiet "${SERVICE_NAME}" 2>/dev/null; then
        record_failure "supervisor-active" required
    fi

    # 3. profile units enabled — REQUIRED.
    local unit
    while IFS= read -r unit; do
        [ -z "${unit}" ] && continue
        if ! unit_enabled "${unit}"; then
            record_failure "unit-enabled:${unit}" required
        fi
    done < <(expected_profile_units "${profile}")

    # 4. agent REST reachable — REQUIRED. wait_for_api_ready already polled
    # in print_status; re-probe quickly here so the gate is self-contained.
    local api_ok=false
    if curl -fsS --max-time 3 http://127.0.0.1:8080/api/status >/dev/null 2>&1; then
        api_ok=true
    elif [ -n "${AGENT_API_VERSION:-}" ]; then
        # print_status already confirmed the API answered this run.
        api_ok=true
    fi
    if [ "${api_ok}" != "true" ]; then
        record_failure "api-reachable" required
    fi

    # Classify overall status. requiredFailures non-empty => failed. Else
    # if any optional step failed => degraded. Else ok.
    local status="ok"
    if [ -n "${REQUIRED_FAILURES// /}" ]; then
        status="failed"
    elif [ -n "${FAILED_STEPS// /}" ]; then
        status="degraded"
    fi

    write_install_result "${status}"

    echo ""
    case "${status}" in
        ok)
            info "Install health gate: OK. All required components are up."
            ;;
        degraded)
            warn "Install health gate: DEGRADED. Required components are up; optional steps failed:${FAILED_STEPS}"
            warn "Result written to ${ADOS_INSTALL_RESULT} (run: ados install --status)."
            ;;
        failed)
            error "Install health gate: FAILED. Required components missing:${REQUIRED_FAILURES}"
            error "Result written to ${ADOS_INSTALL_RESULT} (run: ados install --status)."
            ;;
    esac

    [ "${status}" != "failed" ]
}
