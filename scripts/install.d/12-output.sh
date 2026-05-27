# shellcheck shell=bash
# =============================================================================
# 12-output.sh — install banner / MOTD / readiness probe / summary printers.
#
# Everything the operator sees at the tail end of an install. wait_for_api_ready
# blocks until the agent's REST API answers on localhost:8080. print_status
# is the final dispatcher call and intentionally exits 0 when complete.
# =============================================================================

# Install the SSH login banner at /etc/update-motd.d/30-ados. The
# script reads /api/v1/setup/status with a short timeout and prints
# the primary setup URL plus the `ados` hint. Skipped on non-Linux.
install_motd() {
    [ "$(uname -s)" = "Linux" ] || return 0

    local motd_src=""
    if [ -n "${MOTD_SRC_DIR:-}" ] && [ -d "${MOTD_SRC_DIR}" ]; then
        motd_src="${MOTD_SRC_DIR}"
    elif [ -n "${FRESH_REPO_DIR:-}" ] && [ -d "${FRESH_REPO_DIR}/repo/data/motd" ]; then
        motd_src="${FRESH_REPO_DIR}/repo/data/motd"
    elif [ -d "${INSTALL_DIR}/repo/data/motd" ]; then
        motd_src="${INSTALL_DIR}/repo/data/motd"
    elif [ -d "$(dirname "$0" 2>/dev/null)/../data/motd" ] 2>/dev/null; then
        motd_src="$(cd "$(dirname "$0")/../data/motd" && pwd)"
    fi

    if [ -z "$motd_src" ] || [ ! -f "${motd_src}/30-ados" ]; then
        warn "MOTD source not found, skipping login banner install."
        return 0
    fi

    mkdir -p /etc/update-motd.d
    install -m 0755 "${motd_src}/30-ados" /etc/update-motd.d/30-ados
    info "SSH login banner installed at /etc/update-motd.d/30-ados."
}

# Block until the agent's REST API is reachable on localhost:8080, or the
# timeout expires. Used so install.sh can honor Rule 26: every manual step
# a bench operator might have to perform is a bug. The operator should be
# able to type `ados` immediately after the install returns and have a
# working agent.
#
# On Pi 4B class hardware the API binds at ~70-80s after service start
# because the API process does HAL board fingerprinting, USB enumeration,
# CSI camera scan, profile auto-detect, feature manager init, and IPC
# socket setup before listening on 8080. Lower-RAM SBCs are slower. The
# default timeout is therefore generous (180s); callers that know they
# can be stricter pass a smaller value.
#
# On success this function also captures the running agent's version
# string from the API into the global AGENT_API_VERSION so the
# post-install summary can print it. /api/status is used instead of
# /api/v1/setup/status because the former is a fast snapshot endpoint
# and the latter does additional setup-state introspection that takes
# multiple seconds per call.
AGENT_API_VERSION=""
wait_for_api_ready() {
    [ "$(uname -s)" = "Linux" ] || return 0
    local timeout="${1:-180}"
    local start
    start=$(date +%s)
    local now
    local body
    while :; do
        body=$(curl -fsS --max-time 2 http://127.0.0.1:8080/api/status 2>/dev/null || true)
        if [ -n "${body}" ]; then
            AGENT_API_VERSION=$(printf '%s' "${body}" | python3 -c \
                'import json,sys
try:
    print(json.load(sys.stdin).get("version", ""))
except Exception:
    pass' 2>/dev/null || true)
            if [ -n "${AGENT_API_VERSION}" ]; then
                info "Agent REST API reachable on 127.0.0.1:8080 (version ${AGENT_API_VERSION})."
                return 0
            fi
        fi
        now=$(date +%s)
        if [ $((now - start)) -ge "${timeout}" ]; then
            warn "Agent REST API did not come up within ${timeout}s."
            warn "Last 30 lines of ados-api journal:"
            journalctl -u ados-api -n 30 --no-pager 2>&1 | sed 's/^/  /' || true
            warn "Last 30 lines of ados-supervisor journal:"
            journalctl -u ados-supervisor -n 30 --no-pager 2>&1 | sed 's/^/  /' || true
            return 1
        fi
        sleep 2
    done
}

# ─── Print Pairing Code Box ─────────────────────────────────────────────────

print_pairing_code() {
    local pairing_file="${CONFIG_DIR}/pairing.json"
    if [ -f "$pairing_file" ]; then
        local display_code
        display_code=$(python3 -c "import json; print(json.load(open('${pairing_file}')).get('pairing_code', '------'))" 2>/dev/null || echo "------")
        if [ "$display_code" != "------" ] && [ -n "$display_code" ]; then
            echo ""
            echo -e "  ${BOLD}+----------+${NC}"
            echo -e "  ${BOLD}|  ${display_code}  |${NC}  Pairing Code"
            echo -e "  ${BOLD}+----------+${NC}"
            echo ""
            echo "  Enter this code in ADOS Mission Control to pair with this drone."
            echo "  The agent is beaconing this code to the cloud."
            echo "  If your GCS is open, pairing should complete automatically within 30 seconds."
            echo ""
        fi
    fi
}

# ─── Print Hardware Summary ─────────────────────────────────────────────────

# Force a fresh probe via POST /api/v1/setup/hardware-check/refresh so
# the persisted snapshot reflects whatever was hot-plugged during the
# install, then render the per-item state inline. Keeps the bench
# operator from having to open the GCS just to confirm "all required
# components ok" right after install.
#
# The python parser is invoked via a quoted heredoc (<<'PYEOF') so
# bash performs zero variable / backslash interpolation. This avoids
# escaping pitfalls when the script is delivered through `curl | sudo bash`.
print_hardware_summary() {
    [ "$(uname -s)" = "Linux" ] || return 0

    # Use the agent's own CLI to print the snapshot. The `ados hardware
    # show` subcommand reads /var/ados/setup/hardware-state.json and
    # renders a per-item table identical to what the dashboard shows,
    # which keeps install.sh free of inline Python (curl|bash paths
    # have historically choked on heredoc-inside-command-substitution
    # patterns; sourcing the CLI bypasses all of that).
    local ados_bin="${VENV_DIR}/bin/ados"
    [ -x "${ados_bin}" ] || return 0

    # Force-write a fresh snapshot so we capture whatever was
    # hot-plugged during install. Best-effort.
    curl -fsS --max-time 8 -X POST \
        http://127.0.0.1:8080/api/v1/setup/hardware-check/refresh \
        > /dev/null 2>&1 || true

    local rendered
    rendered=$("${ados_bin}" hardware show 2>/dev/null) || true
    [ -n "${rendered}" ] || return 0

    echo ""
    echo -e "${BOLD}--- Hardware probe ---${NC}"
    printf '%s\n' "${rendered}" | sed 's/^/  /'
}

# ─── Print Status Summary ───────────────────────────────────────────────────

print_status() {
    # Block until the agent is actually serving requests so the operator
    # can run `ados` immediately after this returns. Per Rule 26 every
    # post-install manual step is a bug; if we exit before 8080 is
    # bound, the operator races the supervisor's startup. Pi 4B class
    # hardware needs ~75s; budget plenty of headroom for slower SBCs.
    wait_for_api_ready 180 || true

    print_hardware_summary

    echo ""
    echo -e "${BOLD}=== Installation Complete ===${NC}"
    echo ""
    echo "  Install dir:  ${INSTALL_DIR}"
    echo "  Config:       ${CONFIG_DIR}/config.yaml"
    echo "  Device ID:    ${DEVICE_ID_FILE}"
    echo "  Data:         ${DATA_DIR}/"
    echo "  Venv:         ${VENV_DIR}"
    echo "  Service:      ${SERVICE_NAME}"
    echo ""
    echo "  Start:        sudo systemctl start ${SERVICE_NAME}"
    echo "  Status:       sudo systemctl status ${SERVICE_NAME}"
    echo "  Logs:         journalctl -u ${SERVICE_NAME} -f"
    echo "  CLI:          ados status"
    echo "  TUI:          ados tui"
    echo "  Diagnostics:  ados diag"
    echo ""

    # Print the running agent's version. Prefer the value captured from
    # /api/status during readiness polling; fall back to the package
    # metadata (stable, no subprocess startup overhead) if the API didn't
    # come up in time. The CLI does not expose a `version` subcommand, so
    # we never shell out to `ados version` here.
    local pkg_version=""
    if [ -x "${VENV_DIR}/bin/python" ]; then
        pkg_version=$("${VENV_DIR}/bin/python" -c \
            'from importlib.metadata import version, PackageNotFoundError
try:
    print(version("ados-drone-agent"))
except PackageNotFoundError:
    pass' 2>/dev/null || true)
    fi
    local shown_version="${AGENT_API_VERSION:-${pkg_version:-unknown}}"
    echo "  Version:      ${shown_version}"
    echo ""
}
