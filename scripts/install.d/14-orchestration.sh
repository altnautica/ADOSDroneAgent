# shellcheck shell=bash
# =============================================================================
# 14-orchestration.sh — drop-proof detach, completeness probe, per-step
# checkpoints, and the install success contract + result file.
#
# Sourced last after 13-main.sh so its helpers are in scope before the
# dispatcher decides whether to detach and before main_install_flow runs
# the health gate.
#
# Three concerns live here:
#
#   1. Detach. maybe_reexec_detached re-launches the whole installer under
#      a transient systemd unit (or setsid) so a dropped SSH session can no
#      longer SIGHUP a mid-flight compile and leave a half-installed box.
#
#   2. Completeness + checkpoints. is_install_complete answers "is every
#      REQUIRED component actually present and enabled", not just "is the
#      binary on disk". Per-step checkpoints under
#      /var/lib/ados/install-checkpoints let a resumed run skip finished
#      modules and let `ados install --status` show what is done vs missing.
#
#   3. Success contract. run_health_gate asserts the REQUIRED components are
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
export ADOS_INSTALL_LOG_DIR="${ADOS_INSTALL_LOG_DIR:-/var/log/ados}"

# Source the shared network fetch helpers (ados_fetch / ados_reachable).
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

# checkpoint_run NAME FUNC [ARGS...] — run FUNC once, marking NAME on success.
# When NAME is already marked AND the install is not being forced, skip the
# call and narrate the skip so a resumed run is legible in the journal. The
# step itself stays idempotent, so re-running on a non-resume path is safe;
# the checkpoint is purely an optimisation + an audit trail.
checkpoint_run() {
    local name="$1"; shift
    if [ "${DO_FORCE:-false}" != "true" ] && checkpoint_done "${name}"; then
        info "Step '${name}' already complete (checkpoint present); skipping."
        return 0
    fi
    if "$@"; then
        checkpoint_mark "${name}"
        return 0
    fi
    # Step failed: do NOT mark. A later resume re-runs it. Surface the
    # failure to the caller so REQUIRED-vs-OPTIONAL classification can act.
    return 1
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
        ground_station|ground-station)
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

# ─── Drop-proof detach ───────────────────────────────────────────────────────
#
# Re-exec the entire installer detached from the controlling terminal so a
# dropped SSH session (which delivers SIGHUP to the foreground process
# group) can no longer kill a mid-flight DKMS compile and leave a
# half-installed box. Prefers a transient systemd unit (ados-install) so
# the run survives logout and is followable with journalctl; falls back to
# setsid on non-systemd hosts, teeing output to a timestamped log.
#
# Returns 0 (caller should `return`/exit) when the install was handed off
# to the detached process. Returns 1 when detach is skipped and the caller
# should continue running the install inline. Skip conditions:
#   - --foreground flag or ADOS_INSTALL_FOREGROUND=1
#   - already inside the detached re-exec (ADOS_INSTALL_DETACHED=1)
#   - not attached to an interactive/SSH terminal (cron, CI, image build)
#   - non-Linux (macOS dev mode runs inline)
#   - uninstall (handled inline up the call stack already)
maybe_reexec_detached() {
    # macOS dev installs run inline; no SIGHUP-kills-DKMS problem there.
    [ "$(uname -s)" = "Linux" ] || return 1

    # Already the detached child — run inline so we don't fork forever.
    if [ "${ADOS_INSTALL_DETACHED:-0}" = "1" ]; then
        return 1
    fi

    # Operator opted out.
    if [ "${ADOS_INSTALL_FOREGROUND:-0}" = "1" ] || [ "${DO_FOREGROUND:-false}" = "true" ]; then
        info "Foreground install requested; not detaching."
        return 1
    fi

    # Only detach when there is a terminal that could drop. A pipe-fed
    # bash (curl-pipe) has stdin from the pipe but, post-bootstrap, runs
    # from the cloned tree where the terminal is still the controlling
    # tty; treat presence of a tty on any of stdin/stdout/stderr OR an
    # SSH session as "could be dropped". Headless image builds and CI
    # have none of these and run inline.
    if [ ! -t 0 ] && [ ! -t 1 ] && [ ! -t 2 ] \
        && [ -z "${SSH_CONNECTION:-}" ] && [ -z "${SSH_TTY:-}" ]; then
        return 1
    fi

    # Resolve the on-disk installer to re-exec. By the time we reach the
    # detach point the curl-pipe bootstrap has already cloned + exec'd into
    # a real file, so the dispatcher's resolved path (exported as
    # ADOS_INSTALLER_SELF before it calls us) points at the cloned
    # install.sh. BASH_SOURCE[0] inside a sourced module is this module's
    # path, not install.sh, so it is not usable here. Refuse to detach if
    # the path is unresolvable (correctness over cleverness: better to run
    # inline than to exec the wrong thing).
    local self="${ADOS_INSTALLER_SELF:-}"
    if [ -z "${self}" ] || [ ! -f "${self}" ]; then
        warn "Cannot resolve installer path to detach; running inline."
        return 1
    fi

    # Timestamped log so concurrent or repeated installs do not clobber
    # each other and so the result is auditable after the fact.
    install -d -m 0755 "${ADOS_INSTALL_LOG_DIR}" 2>/dev/null || true
    local stamp logfile
    stamp="$(date -u +%Y%m%dT%H%M%SZ)"
    logfile="${ADOS_INSTALL_LOG_DIR}/install-${stamp}.log"

    # Mark the child so it does not try to detach again.
    local -a child_env=(
        "ADOS_INSTALL_DETACHED=1"
        "ADOS_INSTALL_LOGFILE=${logfile}"
    )
    # Forward the bootstrap dir so the detached child's EXIT trap still
    # cleans the curl-pipe clone (exec does not inherit traps; the child
    # re-registers the trap from ADOS_BOOTSTRAP_DIR at the top of install.sh).
    if [ -n "${ADOS_BOOTSTRAP_DIR:-}" ]; then
        child_env+=("ADOS_BOOTSTRAP_DIR=${ADOS_BOOTSTRAP_DIR}")
    fi
    # Forward profile/branch/display so the detached run resolves the same
    # way without re-reading argv ambiguities (argv is still passed too).
    [ -n "${ADOS_PROFILE:-}" ]  && child_env+=("ADOS_PROFILE=${ADOS_PROFILE}")
    [ -n "${ADOS_DISPLAY:-}" ]  && child_env+=("ADOS_DISPLAY=${ADOS_DISPLAY}")
    [ -n "${ADOS_RELEASE_CHANNEL:-}" ] && child_env+=("ADOS_RELEASE_CHANNEL=${ADOS_RELEASE_CHANNEL}")
    # Carry the release-channel selection + tag pin so the detached child
    # installs the same channel the operator asked for (argv is passed too,
    # but the env keeps it robust against the detached re-exec dropping a
    # flag).
    [ -n "${ADOS_CHANNEL:-}" ]  && child_env+=("ADOS_CHANNEL=${ADOS_CHANNEL}")
    [ -n "${ADOS_VERSION:-}" ]  && child_env+=("ADOS_VERSION=${ADOS_VERSION}")

    if command -v systemd-run >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
        # --collect reaps the unit when it exits so a re-run can reuse the
        # name. --setenv carries our markers into the unit's environment.
        # The unit captures stdout/stderr to the journal automatically;
        # also tee to the log file for parity with the setsid path.
        local -a setenv_args=()
        local kv
        for kv in "${child_env[@]}"; do
            setenv_args+=("--setenv=${kv}")
        done
        info "Detaching install into transient unit 'ados-install' (survives SSH drop)."
        info "Follow with: journalctl -u ados-install -f"
        info "Log file:    ${logfile}"
        # bash -lc wrapper tees the unit's own stdout to the log file.
        # systemd-run --pipe is avoided (it would re-attach to our tty);
        # the default detached unit is exactly what we want.
        if systemd-run --unit=ados-install --collect --service-type=oneshot \
            "${setenv_args[@]}" \
            /usr/bin/env bash -c \
            "exec bash \"\$0\" \"\$@\" > >(tee -a '${logfile}') 2>&1" \
            "${self}" "$@" >/dev/null 2>&1; then
            return 0
        fi
        warn "systemd-run detach failed; falling back to setsid."
    fi

    # Non-systemd / fallback: setsid disowns from the controlling terminal
    # so SIGHUP on terminal close is not delivered. Redirect all stdio to
    # the log; the operator follows with tail -f.
    if command -v setsid >/dev/null 2>&1; then
        info "Detaching install via setsid (survives SSH drop)."
        info "Follow with: tail -f ${logfile}"
        setsid /usr/bin/env "${child_env[@]}" \
            bash "${self}" "$@" </dev/null >>"${logfile}" 2>&1 &
        return 0
    fi

    warn "Neither systemd-run nor setsid available; running install inline (SSH drop will interrupt)."
    return 1
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
# dispatcher (and the detached unit's exit code) reflect the real outcome.
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
