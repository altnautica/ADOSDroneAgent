#!/usr/bin/env bats
# =============================================================================
# Bats test suite for the main install.sh dispatcher.
#
# Asserts:
#   - lib.sh + every install.d/NN-*.sh module sources cleanly into a
#     fresh shell with no errors
#   - every function declared in the spec map is in scope after sourcing
#   - the dispatcher arg parser (extracted as a fragment) accepts the
#     documented flag surface (--upgrade, --force, --pair, --branch,
#     --name, --display, positional CODE)
#   - shared globals (ADOS_PROFILE, BRANCH_NAME, PAIR_CODE, etc.) are
#     exported by the dispatcher before the main install flow runs
#   - no module redefines lib.sh's canonical path constants
#
# Tests do NOT execute the install body — we extract the arg-parsing
# block out of install.sh by line range and run it against synthetic
# argv. The line-range probe is fragile if install.sh changes shape;
# that is intentional. A change to the arg parser should land here too.
# =============================================================================

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    DISPATCHER="${REPO_ROOT}/scripts/install.sh"
    INSTALL_D="${REPO_ROOT}/scripts/install.d"
    [ -f "${DISPATCHER}" ] || {
        echo "missing dispatcher: ${DISPATCHER}" >&2
        return 1
    }
    [ -d "${INSTALL_D}" ] || {
        echo "missing install.d/ directory: ${INSTALL_D}" >&2
        return 1
    }
}

# -----------------------------------------------------------------------------
# Module sourcing + function presence
# -----------------------------------------------------------------------------

@test "lib.sh sources cleanly and exports info/warn/error" {
    run bash -c "source '${INSTALL_D}/lib.sh' && declare -F info warn error >/dev/null && echo OK"
    [ "$status" -eq 0 ]
    [[ "$output" == *"OK"* ]]
}

@test "lib.sh exports the canonical path constants" {
    run bash -c "source '${INSTALL_D}/lib.sh' && echo \"\${INSTALL_DIR} \${CONFIG_DIR} \${DATA_DIR} \${VENV_DIR} \${SERVICE_NAME}\""
    [ "$status" -eq 0 ]
    [[ "$output" == "/opt/ados /etc/ados /var/ados /opt/ados/venv ados-supervisor" ]]
}

@test "lib.sh exports REPO_URL, CONVEX_URL, MEDIAMTX_VERSION, DEVICE_ID_FILE" {
    run bash -c "source '${INSTALL_D}/lib.sh' && echo \"\${REPO_URL} \${CONVEX_URL} \${MEDIAMTX_VERSION} \${DEVICE_ID_FILE}\""
    [ "$status" -eq 0 ]
    [[ "$output" == *"github.com/altnautica/ADOSDroneAgent.git"* ]]
    [[ "$output" == *"convex-site.altnautica.com"* ]]
    [[ "$output" == *"1.17.1"* ]]
    [[ "$output" == *"/etc/ados/device-id"* ]]
}

@test "every install.d/NN-*.sh module sources cleanly after lib.sh" {
    run bash -c "
        set -e
        source '${INSTALL_D}/lib.sh'
        for m in 00-detect 01-state 02-deps 03-kernel 04-dkms 05-mesh \
                 06-radio 07-systemd 08-plugin 09-config 10-network \
                 11-artifacts 12-output 14-orchestration; do
            source '${INSTALL_D}/'\$m.sh
        done
        echo OK
    "
    [ "$status" -eq 0 ]
    [[ "$output" == *"OK"* ]]
}

@test "13-main.sh sources cleanly after other modules" {
    run bash -c "
        set -e
        source '${INSTALL_D}/lib.sh'
        for m in 00-detect 01-state 02-deps 03-kernel 04-dkms 05-mesh \
                 06-radio 07-systemd 08-plugin 09-config 10-network \
                 11-artifacts 12-output 13-main 14-orchestration; do
            source '${INSTALL_D}/'\$m.sh
        done
        echo OK
    "
    [ "$status" -eq 0 ]
    [[ "$output" == *"OK"* ]]
}

@test "main_install_flow is defined after sourcing 13-main.sh" {
    run bash -c "
        source '${INSTALL_D}/lib.sh'
        source '${INSTALL_D}/13-main.sh'
        declare -F main_install_flow >/dev/null && echo OK
    "
    [ "$status" -eq 0 ]
    [[ "$output" == *"OK"* ]]
}

@test "dispatcher sources 14-orchestration last in the module loop" {
    run awk '/for module in/,/; do$/ {printf "%s ", $0} /; do$/ {print ""}' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [[ "$output" == *"14-orchestration"* ]]
    # 14-orchestration must come after 13-main so its helpers are in scope.
    [[ "$output" == *"13-main 14-orchestration"* ]]
}

@test "dispatcher sources 13-main in the module loop" {
    # The `for module in ...; do` block spans multiple lines via `\`
    # continuation, so grep line-by-line cannot see both anchors at once.
    # Use awk to flatten the dispatcher into a logical-line stream and
    # then look for the module loop with 13-main present.
    run awk '/for module in/,/; do$/ {printf "%s ", $0} /; do$/ {print ""}' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [[ "$output" == *"for module in"* ]]
    [[ "$output" == *"13-main"* ]]
    [[ "$output" == *"; do"* ]]

    # Sanity: the dispatcher also calls main_install_flow once after the
    # export line, otherwise the install never runs.
    run grep -E '^main_install_flow[[:space:]]*$' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}

@test "all spec-mapped functions resolve after sourcing modules" {
    run bash -c "
        set -e
        source '${INSTALL_D}/lib.sh'
        for m in 00-detect 01-state 02-deps 03-kernel 04-dkms 05-mesh \
                 06-radio 07-systemd 08-plugin 09-config 10-network \
                 11-artifacts 12-output 14-orchestration; do
            source '${INSTALL_D}/'\$m.sh
        done
        missing=0
        for fn in \
            detect_arch detect_os find_python resolve_profile _persist_profile_to_config \
            detect_profile is_installed get_installed_version do_uninstall \
            install_global_symlinks install_system_deps install_ground_station_deps \
            install_video_sysctl install_display_driver install_ground_station_driver \
            install_mesh_deps install_wfb_ng_from_vendor provision_wfb_bind_artifacts \
            install_systemd_service disable_other_profile_units enable_universal_units \
            mask_conflicting_standalone_services enable_ground_station_units \
            install_plugin_slice install_plugin_tmpfiles generate_device_id \
            generate_default_config harden_secret_perms provision_plugin_keys \
            write_pairing install_mediamtx persist_repo_artifacts install_motd \
            wait_for_api_ready print_pairing_code print_hardware_summary print_status \
            checkpoint_mark checkpoint_done checkpoint_clear checkpoint_run \
            list_completed_checkpoints expected_profile_units unit_enabled \
            is_install_complete maybe_reexec_detached write_install_result \
            record_failure run_health_gate git_clone_retry install_radio_driver_tracked; do
            if ! declare -F \"\$fn\" >/dev/null; then
                echo \"MISSING: \$fn\"
                missing=\$((missing+1))
            fi
        done
        echo \"MISSING_COUNT=\${missing}\"
    "
    [ "$status" -eq 0 ]
    [[ "$output" == *"MISSING_COUNT=0"* ]]
}

# -----------------------------------------------------------------------------
# Arg-parsing probes
#
# Approach: extract the dispatcher arg-parse block from install.sh by
# locating the marker lines, source the extracted snippet in a clean
# shell with stub `error` + `warn` + `do_uninstall`, and dump the
# parsed state. This avoids invoking install.sh proper (which calls
# detect_profile, downloads manifest, etc.) but stays faithful to the
# real parser because the bytes come from the same file.
# -----------------------------------------------------------------------------

extract_arg_parser() {
    # Pull the lines between the "Flag Parsing" comment and the end of
    # the parser loop. The marker is a comment string; if install.sh
    # ever changes shape the test will fail loud and obvious.
    awk '
        /^# ─── Flag Parsing/        { capture = 1 }
        capture { print }
        capture && /^done[[:space:]]*$/ { exit }
    ' "${DISPATCHER}"
}

probe_args() {
    # $@ = argv to feed to the parser. Returns "K=V K=V K=V" of the
    # captured state on stdout.
    local snippet
    snippet="$(extract_arg_parser)"
    # The parser calls do_uninstall on --uninstall and error on missing
    # args. Stub both so the probe never aborts.
    bash -c "
        set -u
        info()  { :; }
        warn()  { :; }
        error() { echo \"ERROR:\$*\" >&2; }
        do_uninstall() { echo 'UNINSTALL_CALLED'; exit 0; }
        # The Flag Parsing block in install.sh assumes BRANCH_NAME is
        # already declared (initialized earlier in the dispatcher, in
        # the 'Full-Agent Install: shared state' section). Mirror those
        # initializations here so the extracted snippet runs cleanly
        # under \`set -u\`.
        BRANCH_NAME=\"\"
        FRESH_REPO_DIR=\"\"
        ADOS_PROFILE=\"\"
        ${snippet}
        echo \"FORCE=\${DO_FORCE} UPGRADE=\${DO_UPGRADE} PAIR=\${PAIR_CODE} NAME=\${DRONE_NAME} BRANCH=\${BRANCH_NAME} DISPLAY=\${ADOS_DISPLAY:-}\"
    " probe_argv "$@"
}

@test "--upgrade sets DO_UPGRADE=true" {
    output="$(probe_args --upgrade)"
    [[ "$output" == *"UPGRADE=true"* ]]
    [[ "$output" == *"FORCE=false"* ]]
}

@test "--force sets DO_FORCE=true" {
    output="$(probe_args --force)"
    [[ "$output" == *"FORCE=true"* ]]
    [[ "$output" == *"UPGRADE=false"* ]]
}

@test "--pair CODE captures the pairing code" {
    output="$(probe_args --pair ABC123)"
    [[ "$output" == *"PAIR=ABC123"* ]]
}

@test "positional CODE captures the pairing code without --pair" {
    output="$(probe_args XYZ789)"
    [[ "$output" == *"PAIR=XYZ789"* ]]
}

@test "positional non-code arg does not populate PAIR_CODE" {
    output="$(probe_args --force)"
    [[ "$output" == *"PAIR="* ]]
    [[ "$output" != *"PAIR=--force"* ]]
}

@test "--branch NAME captures the feature branch" {
    output="$(probe_args --branch feature/foo)"
    [[ "$output" == *"BRANCH=feature/foo"* ]]
}

@test "--name NAME captures the drone name" {
    output="$(probe_args --name skynode)"
    [[ "$output" == *"NAME=skynode"* ]]
}

@test "--display VALUE exports ADOS_DISPLAY" {
    output="$(probe_args --display waveshare35a)"
    [[ "$output" == *"DISPLAY=waveshare35a"* ]]
}

@test "--uninstall hits do_uninstall fast path" {
    output="$(probe_args --uninstall 2>&1)"
    [[ "$output" == *"UNINSTALL_CALLED"* ]]
}

@test "combined --upgrade --pair CODE captures both" {
    output="$(probe_args --upgrade --pair ABC123)"
    [[ "$output" == *"UPGRADE=true"* ]]
    [[ "$output" == *"PAIR=ABC123"* ]]
}

# -----------------------------------------------------------------------------
# Sourcing-order regression: lib.sh constants must precede module use
# -----------------------------------------------------------------------------

@test "module bodies do not redefine lib.sh canonical path constants" {
    # Each module is allowed to read VENV_DIR / CONFIG_DIR / etc. but
    # must not re-export the canonical constants. A redefinition would
    # mask a future lib.sh change. Sentinel: grep for `export CONST=`
    # next to the canonical names; the only legal occurrence is in
    # lib.sh itself.
    run bash -c "
        grep -rlE '^export (INSTALL_DIR|CONFIG_DIR|DATA_DIR|VENV_DIR|SERVICE_NAME|REPO_URL|DEVICE_ID_FILE|CONVEX_URL|MEDIAMTX_VERSION)=' \
            '${INSTALL_D}' 2>/dev/null \
        | grep -v '/lib\\.sh\$' \
        | wc -l \
        | tr -d ' '
    "
    [ "$status" -eq 0 ]
    [ "$output" = "0" ]
}

# -----------------------------------------------------------------------------
# Dispatcher main flow exports its shared globals
# -----------------------------------------------------------------------------

@test "dispatcher exports ADOS_PROFILE / BRANCH_NAME / PAIR_CODE / DRONE_NAME before main flow" {
    # Look for an explicit `export ADOS_PROFILE BRANCH_NAME PAIR_CODE`
    # line in install.sh. The dispatcher contract requires shared
    # globals to be exported AFTER arg parsing and BEFORE the main
    # install body, so all sourced module functions see the same values.
    run bash -c "grep -E '^export.*ADOS_PROFILE.*BRANCH_NAME.*PAIR_CODE.*DRONE_NAME' '${DISPATCHER}'"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}

@test "dispatcher exports FRESH_REPO_DIR and SYSTEMD_SRC_DIR" {
    # FRESH_REPO_DIR is read by install_systemd_service / install_motd /
    # install_plugin_tmpfiles / persist_repo_artifacts via subshells
    # spawned out of the dispatcher main flow. Must be exported for
    # the subshells to see it. SYSTEMD_SRC_DIR same story.
    #
    # FRESH_REPO_DIR is exported up front in install.sh alongside the
    # other shared globals. SYSTEMD_SRC_DIR is set + exported inside
    # the main install flow (13-main.sh) because the path is only
    # known once the fresh-clone branch picks a temp directory.
    # Search both files so we catch the post-split layout.
    run grep -rE '^[[:space:]]*export[[:space:]]+.*(FRESH_REPO_DIR|SYSTEMD_SRC_DIR)' \
        "${DISPATCHER}" "${INSTALL_D}/13-main.sh"
    [ "$status" -eq 0 ]
    [[ "$output" == *"FRESH_REPO_DIR"* ]]
    [[ "$output" == *"SYSTEMD_SRC_DIR"* ]]
}

@test "dispatcher sources lib.sh first then every numbered module" {
    # The source order is fixed: lib.sh must come before any NN-*.sh
    # because lib.sh defines info/warn/error + path constants.
    run bash -c "grep -nE 'source.*install\\.d/' '${DISPATCHER}' | head -3"
    [ "$status" -eq 0 ]
    # First source must be lib.sh.
    first_source="$(echo "$output" | head -1)"
    [[ "$first_source" == *"lib.sh"* ]]
}

# -----------------------------------------------------------------------------
# curl-pipe bootstrap (regression: install.d/* modules must reach the rig)
# -----------------------------------------------------------------------------

@test "dispatcher self-bootstraps when curl-piped (ADOS_SCRIPT_DIR empty)" {
    # The dispatcher contains a bootstrap block: when BASH_SOURCE[0] is
    # not a real file (curl-pipe-to-bash), the install.d/*.sh modules
    # were never sourced. The bootstrap must git-clone the repo and
    # exec install.sh from there BEFORE the lite dispatch tries to call
    # detect_profile (which lives in 00-detect.sh).
    run grep -nE "curl-pipe bootstrap" "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
    # And the bootstrap must come before the lite dispatch block.
    bootstrap_line="$(grep -nE 'curl-pipe bootstrap' "${DISPATCHER}" | head -1 | cut -d: -f1)"
    lite_line="$(grep -nE 'Lite-rs Profile Pre-dispatch' "${DISPATCHER}" | head -1 | cut -d: -f1)"
    [ -n "$bootstrap_line" ]
    [ -n "$lite_line" ]
    [ "$bootstrap_line" -lt "$lite_line" ]
}

@test "bootstrap re-execs into the cloned repo's install.sh" {
    # The bootstrap should end with exec'ing the cloned install.sh,
    # passing through the original args so --pair / --upgrade / --branch
    # behave the same as a non-piped invocation.
    run bash -c "awk '/curl-pipe bootstrap/,/^fi$/' '${DISPATCHER}' | grep -cE 'exec.*install\\.sh.*\\\"\\\$@\\\"'"
    [ "$status" -eq 0 ]
    [ "$output" = "1" ]
}

# -----------------------------------------------------------------------------
# Orchestration: checkpoints, completeness probe, detach skip, result file,
# health gate exit code.
#
# These exercise the 14-orchestration.sh helpers in isolation against a
# temp state dir, with systemctl / curl / the venv python stubbed so the
# probes are deterministic on this CI host (which has no /opt/ados, no
# systemd units, and is not the target SBC). The harness writes a small
# driver script that sources lib.sh + 14-orchestration.sh with the contract
# paths redirected under a temp tree, then runs the function under test.
# -----------------------------------------------------------------------------

orch_setup() {
    ORCH_TMP="$(mktemp -d)"
    ORCH_BIN="${ORCH_TMP}/bin"
    mkdir -p "${ORCH_BIN}"
    mkdir -p "${ORCH_TMP}/venv/bin"
    printf '#!/usr/bin/env bash\nexit 0\n' > "${ORCH_TMP}/venv/bin/ados"
    chmod +x "${ORCH_TMP}/venv/bin/ados"
    mkdir -p "${ORCH_TMP}/site/ados"
    : > "${ORCH_TMP}/site/ados/__init__.py"
    cat > "${ORCH_TMP}/venv/bin/python" <<PYEOF
#!/usr/bin/env bash
exec /usr/bin/env PYTHONPATH="${ORCH_TMP}/site" python3 "\$@"
PYEOF
    chmod +x "${ORCH_TMP}/venv/bin/python"
}

orch_teardown() {
    [ -n "${ORCH_TMP:-}" ] && rm -rf "${ORCH_TMP}"
}

orch_run() {
    local snippet="$1"
    bash -c "
        set -uo pipefail
        export PATH='${ORCH_BIN}:'\$PATH
        source '${INSTALL_D}/lib.sh'
        export ADOS_STATE_DIR='${ORCH_TMP}/state'
        export ADOS_CHECKPOINT_DIR='${ORCH_TMP}/state/install-checkpoints'
        export ADOS_INSTALL_RESULT='${ORCH_TMP}/state/install-result.json'
        export ADOS_INSTALL_LOG_DIR='${ORCH_TMP}/log'
        export VENV_DIR='${ORCH_TMP}/venv'
        export SERVICE_NAME='ados-supervisor'
        source '${INSTALL_D}/14-orchestration.sh'
        ${snippet}
    "
}

@test "checkpoint_mark then checkpoint_done round-trips" {
    orch_setup
    run orch_run "checkpoint_mark deps && checkpoint_done deps && echo DONE"
    orch_teardown
    [ "$status" -eq 0 ]
    [[ "$output" == *"DONE"* ]]
}

@test "checkpoint_done is false before a mark" {
    orch_setup
    run orch_run "checkpoint_done deps && echo YES || echo NO"
    orch_teardown
    [[ "$output" == *"NO"* ]]
}

@test "checkpoint_clear removes all markers" {
    orch_setup
    run orch_run "checkpoint_mark a; checkpoint_mark b; checkpoint_clear; checkpoint_done a && echo HASA || echo NOA"
    orch_teardown
    [[ "$output" == *"NOA"* ]]
}

@test "list_completed_checkpoints reports marked steps" {
    orch_setup
    run orch_run "checkpoint_mark deps; checkpoint_mark venv; list_completed_checkpoints"
    orch_teardown
    [[ "$output" == *"deps"* ]]
    [[ "$output" == *"venv"* ]]
}

@test "checkpoint_run skips a marked step on a non-force run" {
    orch_setup
    run orch_run "
        DO_FORCE=false
        ran=0
        work() { ran=1; }
        checkpoint_mark slowstep
        checkpoint_run slowstep work
        echo RAN=\$ran
    "
    orch_teardown
    [[ "$output" == *"RAN=0"* ]]
}

@test "checkpoint_run runs an unmarked step and marks it" {
    orch_setup
    run orch_run "
        DO_FORCE=false
        work() { return 0; }
        checkpoint_run freshstep work && checkpoint_done freshstep && echo MARKED
    "
    orch_teardown
    [[ "$output" == *"MARKED"* ]]
}

@test "checkpoint_run does NOT mark a failing step" {
    orch_setup
    run orch_run "
        DO_FORCE=false
        work() { return 7; }
        checkpoint_run failstep work || true
        checkpoint_done failstep && echo MARKED || echo UNMARKED
    "
    orch_teardown
    [[ "$output" == *"UNMARKED"* ]]
}

@test "expected_profile_units lists only supervisor for drone" {
    orch_setup
    run orch_run "expected_profile_units drone"
    orch_teardown
    [[ "$output" == *"ados-supervisor.service"* ]]
    [[ "$output" != *"ados-wfb-rx.service"* ]]
}

@test "expected_profile_units lists GS units for ground_station" {
    orch_setup
    run orch_run "expected_profile_units ground_station"
    orch_teardown
    [[ "$output" == *"ados-supervisor.service"* ]]
    [[ "$output" == *"ados-wfb-rx.service"* ]]
    [[ "$output" == *"ados-hostapd.service"* ]]
}

@test "is_install_complete is FALSE when venv import fails" {
    orch_setup
    # Replace the venv python with a stub that fails the ados import. The
    # real ados is on this host's path via the editable install, so simply
    # clearing the shim site dir would not make `import ados` fail; the stub
    # returns non-zero for `-c "import ados"` and succeeds for anything else.
    cat > "${ORCH_TMP}/venv/bin/python" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = "-c" ] && printf '%s' "$2" | grep -q 'import ados'; then
    exit 1
fi
exit 0
EOF
    chmod +x "${ORCH_TMP}/venv/bin/python"
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    cat > "${ORCH_BIN}/systemctl" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "is-enabled" ] && { echo enabled; exit 0; }
exit 0
EOF
    chmod +x "${ORCH_BIN}/uname" "${ORCH_BIN}/systemctl"
    run orch_run "
        is_install_complete drone || true
        echo \"MISSING=[\${INSTALL_MISSING}]\"
    "
    orch_teardown
    [[ "$output" == *"venv-import"* ]]
}

@test "is_install_complete is FALSE when a profile unit is not enabled" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    cat > "${ORCH_BIN}/systemctl" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "is-enabled" ] && { echo disabled; exit 1; }
exit 0
EOF
    chmod +x "${ORCH_BIN}/uname" "${ORCH_BIN}/systemctl"
    run orch_run "is_install_complete drone || true; echo \"MISSING=[\${INSTALL_MISSING}]\""
    orch_teardown
    [[ "$output" == *"unit:ados-supervisor.service"* ]]
}

@test "maybe_reexec_detached is SKIPPED under --foreground" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    chmod +x "${ORCH_BIN}/uname"
    run orch_run "DO_FOREGROUND=true; maybe_reexec_detached && echo DETACHED || echo INLINE"
    orch_teardown
    [[ "$output" == *"INLINE"* ]]
}

@test "maybe_reexec_detached is SKIPPED when already detached" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    chmod +x "${ORCH_BIN}/uname"
    run orch_run "ADOS_INSTALL_DETACHED=1; maybe_reexec_detached && echo DETACHED || echo INLINE"
    orch_teardown
    [[ "$output" == *"INLINE"* ]]
}

@test "maybe_reexec_detached is SKIPPED on non-Linux" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Darwin; exit 0; }
exec /usr/bin/uname "$@"
EOF
    chmod +x "${ORCH_BIN}/uname"
    run orch_run "maybe_reexec_detached && echo DETACHED || echo INLINE"
    orch_teardown
    [[ "$output" == *"INLINE"* ]]
}

@test "write_install_result writes JSON with status ok and ISO timestamp" {
    orch_setup
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 9.9.9; }
        write_install_result ok
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [ "$status" -eq 0 ]
    [[ "$output" == *'"status": "ok"'* ]]
    [[ "$output" == *'"version": "9.9.9"'* ]]
    [[ "$output" == *'"profile": "drone"'* ]]
    [[ "$output" =~ [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z ]]
}

@test "write_install_result records failedSteps and requiredFailures arrays" {
    orch_setup
    run orch_run "
        ADOS_PROFILE=ground_station
        get_installed_version() { echo 1.2.3; }
        record_failure radio-driver optional
        record_failure supervisor-active required
        write_install_result failed
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [ "$status" -eq 0 ]
    [[ "$output" == *'"status": "failed"'* ]]
    [[ "$output" == *"radio-driver"* ]]
    [[ "$output" == *"supervisor-active"* ]]
}

@test "run_health_gate returns NON-ZERO and writes failed when supervisor is down" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    cat > "${ORCH_BIN}/systemctl" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  is-active) exit 3 ;;
  is-enabled) echo enabled; exit 0 ;;
esac
exit 0
EOF
    cat > "${ORCH_BIN}/curl" <<'EOF'
#!/usr/bin/env bash
exit 7
EOF
    chmod +x "${ORCH_BIN}/uname" "${ORCH_BIN}/systemctl" "${ORCH_BIN}/curl"
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 1.0.0; }
        run_health_gate && echo GATE_OK || echo GATE_FAIL
        echo '---'
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [[ "$output" == *"GATE_FAIL"* ]]
    [[ "$output" == *'"status": "failed"'* ]]
    [[ "$output" == *"supervisor-active"* ]]
}

@test "run_health_gate returns ZERO and writes ok when all required pass" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    cat > "${ORCH_BIN}/systemctl" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  is-active) exit 0 ;;
  is-enabled) echo enabled; exit 0 ;;
esac
exit 0
EOF
    cat > "${ORCH_BIN}/curl" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "${ORCH_BIN}/uname" "${ORCH_BIN}/systemctl" "${ORCH_BIN}/curl"
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 1.0.0; }
        run_health_gate && echo GATE_OK || echo GATE_FAIL
        echo '---'
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [[ "$output" == *"GATE_OK"* ]]
    [[ "$output" == *'"status": "ok"'* ]]
}

@test "run_health_gate returns ZERO with degraded when only optional fails" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    cat > "${ORCH_BIN}/systemctl" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  is-active) exit 0 ;;
  is-enabled) echo enabled; exit 0 ;;
esac
exit 0
EOF
    cat > "${ORCH_BIN}/curl" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "${ORCH_BIN}/uname" "${ORCH_BIN}/systemctl" "${ORCH_BIN}/curl"
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 1.0.0; }
        record_failure radio-driver optional
        run_health_gate && echo GATE_OK || echo GATE_FAIL
        echo '---'
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [[ "$output" == *"GATE_OK"* ]]
    [[ "$output" == *'"status": "degraded"'* ]]
    [[ "$output" == *"radio-driver"* ]]
}

# -----------------------------------------------------------------------------
# Source-shape regressions for the resume gate + detach wiring + health gate.
# -----------------------------------------------------------------------------

@test "13-main resumes when is_install_complete is false" {
    run grep -nE "is_install_complete" "${INSTALL_D}/13-main.sh"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
    run grep -nE "ADOS_RESUME=true" "${INSTALL_D}/13-main.sh"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}

@test "13-main full-install + upgrade paths end with run_health_gate" {
    run grep -cE "run_health_gate" "${INSTALL_D}/13-main.sh"
    [ "$status" -eq 0 ]
    [ "$output" -ge 2 ]
}

@test "dispatcher detaches before main_install_flow" {
    run grep -nE "maybe_reexec_detached" "${DISPATCHER}"
    [ "$status" -eq 0 ]
    detach_line="$(grep -nE 'maybe_reexec_detached "\$@"' "${DISPATCHER}" | head -1 | cut -d: -f1)"
    flow_line="$(grep -nE '^main_install_flow[[:space:]]*$' "${DISPATCHER}" | head -1 | cut -d: -f1)"
    [ -n "$detach_line" ]
    [ -n "$flow_line" ]
    [ "$detach_line" -lt "$flow_line" ]
}

@test "dispatcher accepts --foreground flag" {
    run grep -nE '\-\-foreground\)' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}
