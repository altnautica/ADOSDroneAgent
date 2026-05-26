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
                 11-artifacts 12-output 14-orchestration 15-channel; do
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
                 11-artifacts 12-output 13-main 14-orchestration 15-channel; do
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
                 11-artifacts 12-output 14-orchestration 15-channel; do
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
            is_install_complete write_install_result \
            record_failure run_health_gate git_clone_retry install_radio_driver_tracked \
            resolve_channel is_stable_channel stable_pubkey_or_empty resolve_stable_tag \
            stable_version_from_tag stable_wheel_name stable_bundle_name stable_asset_base \
            fetch_and_verify_stable_asset fetch_and_verify_stable_assets unpack_deploy_bundle \
            install_agent_from_wheel print_channel_banner show_stable_key; do
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
        # show_stable_key is only defined when 15-channel.sh is sourced; the
        # extracted snippet's --show-key branch calls it, so stub it here so
        # the parser fragment runs standalone.
        show_stable_key() { echo 'SHOW_KEY_CALLED'; }
        ${snippet}
        echo \"FORCE=\${DO_FORCE} UPGRADE=\${DO_UPGRADE} PAIR=\${PAIR_CODE} NAME=\${DRONE_NAME} BRANCH=\${BRANCH_NAME} DISPLAY=\${ADOS_DISPLAY:-} CHANNEL=\${ADOS_CHANNEL:-} VERSION=\${ADOS_VERSION:-}\"
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

@test "--channel stable sets ADOS_CHANNEL=stable" {
    output="$(probe_args --channel stable)"
    [[ "$output" == *"CHANNEL=stable"* ]]
}

@test "--channel edge sets ADOS_CHANNEL=edge" {
    output="$(probe_args --channel edge)"
    [[ "$output" == *"CHANNEL=edge"* ]]
}

@test "--channel rejects an unknown value" {
    run probe_args --channel bogus
    [[ "$output" == *"ERROR:"* ]] || [ "$status" -ne 0 ]
}

@test "--version pins the tag value" {
    output="$(probe_args --version 0.40.4)"
    [[ "$output" == *"VERSION=0.40.4"* ]]
}

@test "no channel flag leaves ADOS_CHANNEL unset (dispatcher defaults to edge)" {
    output="$(probe_args --force)"
    [[ "$output" == *"CHANNEL="* ]]
    [[ "$output" != *"CHANNEL=stable"* ]]
}

@test "--show-key hits the show_stable_key path" {
    output="$(probe_args --show-key 2>&1)"
    [[ "$output" == *"SHOW_KEY_CALLED"* ]]
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
# Orchestration: checkpoints, completeness probe, result file,
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
# Source-shape regressions for the resume gate + inline install + health gate.
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

@test "dispatcher runs main_install_flow inline with no re-exec/detach machinery" {
    # The installer runs in the foreground in this process. There must be no
    # re-exec into a transient unit or a backgrounded session: no systemd-run,
    # no setsid, no maybe_reexec_detached anywhere in the dispatcher or its
    # modules, and the dispatcher must call main_install_flow directly.
    run grep -rnE 'maybe_reexec_detached|systemd-run|setsid' \
        "${DISPATCHER}" "${INSTALL_D}/14-orchestration.sh"
    [ "$status" -ne 0 ]
    [ -z "$output" ]

    # The dispatcher calls main_install_flow exactly once, inline.
    run grep -cE '^main_install_flow[[:space:]]*$' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ "$output" = "1" ]
}

@test "dispatcher accepts --channel and --version flags" {
    run grep -nE '\-\-channel\)' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
    run grep -nE '\-\-version\)' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}

@test "dispatcher defaults ADOS_CHANNEL to edge" {
    # The dispatcher exports ADOS_CHANNEL with an edge default after arg
    # parsing so the channel selection survives into the main flow.
    run grep -nE '^export ADOS_CHANNEL="\$\{ADOS_CHANNEL:-edge\}"' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}

@test "dispatcher sources 15-channel after 14-orchestration in the module loop" {
    run awk '/for module in/,/; do$/ {printf "%s ", $0} /; do$/ {print ""}' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    [[ "$output" == *"14-orchestration 15-channel"* ]]
}

@test "13-main full-install branches on the stable channel" {
    run grep -cE "is_stable_channel" "${INSTALL_D}/13-main.sh"
    [ "$status" -eq 0 ]
    # fresh-install + upgrade + ground-station extras all branch on channel.
    [ "$output" -ge 3 ]
    run grep -nE "fetch_and_verify_stable_assets|install_agent_from_wheel|unpack_deploy_bundle" "${INSTALL_D}/13-main.sh"
    [ "$status" -eq 0 ]
    [ -n "$output" ]
}

# -----------------------------------------------------------------------------
# Channel module (15-channel.sh): channel resolution, tag resolution, stable
# fetch+verify path, wheel install path. The 15-channel module sources
# scripts/lib/verify.sh on its own, so we source it standalone over lib.sh +
# 14-orchestration and mock ados_fetch / ados_verify_artifact / pip so the
# probes are deterministic on a host with no network and no /opt/ados.
# -----------------------------------------------------------------------------

chan_setup() {
    CHAN_TMP="$(mktemp -d)"
    CHAN_BIN="${CHAN_TMP}/bin"
    mkdir -p "${CHAN_BIN}"
    mkdir -p "${CHAN_TMP}/venv/bin"
    # A pip stub that records its argv so the wheel-vs-source path is testable.
    cat > "${CHAN_TMP}/venv/bin/pip" <<EOF
#!/usr/bin/env bash
echo "PIP_ARGS: \$*" >> "${CHAN_TMP}/pip.log"
exit 0
EOF
    chmod +x "${CHAN_TMP}/venv/bin/pip"
}

chan_teardown() {
    [ -n "${CHAN_TMP:-}" ] && rm -rf "${CHAN_TMP}"
}

chan_run() {
    local snippet="$1"
    bash -c "
        set -uo pipefail
        export PATH='${CHAN_BIN}:'\$PATH
        source '${INSTALL_D}/lib.sh'
        export VENV_DIR='${CHAN_TMP}/venv'
        source '${INSTALL_D}/14-orchestration.sh'
        source '${INSTALL_D}/15-channel.sh'
        ${snippet}
    "
}

@test "resolve_channel defaults to edge with no env" {
    chan_setup
    run chan_run "unset ADOS_CHANNEL 2>/dev/null; resolve_channel"
    chan_teardown
    [[ "$output" == *"edge"* ]]
    [[ "$output" != *"stable"* ]]
}

@test "resolve_channel honors ADOS_CHANNEL=stable" {
    chan_setup
    run chan_run "export ADOS_CHANNEL=stable; resolve_channel"
    chan_teardown
    [[ "$output" == *"stable"* ]]
}

@test "resolve_channel normalizes a typo back to edge" {
    chan_setup
    run chan_run "export ADOS_CHANNEL=stabel; resolve_channel"
    chan_teardown
    [[ "$output" == *"edge"* ]]
}

@test "resolve_stable_tag prefers an explicit X.Y.Z pin and prefixes v" {
    chan_setup
    run chan_run "export ADOS_VERSION=0.40.4; resolve_stable_tag"
    chan_teardown
    [[ "$output" == *"v0.40.4"* ]]
}

@test "resolve_stable_tag passes through a vX.Y.Z pin verbatim" {
    chan_setup
    run chan_run "export ADOS_VERSION=v1.2.3; resolve_stable_tag"
    chan_teardown
    [[ "$output" == "v1.2.3" ]]
}

@test "resolve_stable_tag reads the latest v* tag from the releases API" {
    chan_setup
    run chan_run "
        ados_fetch() { printf '%s\n' '[{\"tag_name\": \"v0.41.0\"}, {\"tag_name\": \"v0.40.4\"}, {\"tag_name\": \"lite-v0.1.5\"}]'; }
        unset ADOS_VERSION 2>/dev/null
        resolve_stable_tag
    "
    chan_teardown
    [[ "$output" == *"v0.41.0"* ]]
    [[ "$output" != *"lite"* ]]
}

@test "stable_wheel_name + stable_bundle_name match the release naming" {
    chan_setup
    run chan_run "stable_wheel_name 0.40.4; stable_bundle_name 0.40.4"
    chan_teardown
    [[ "$output" == *"ados_drone_agent-0.40.4-py3-none-any.whl"* ]]
    [[ "$output" == *"ados-drone-agent-deploy-0.40.4.tar.gz"* ]]
}

@test "stable channel REFUSES when the embedded key is still the placeholder" {
    chan_setup
    # With the placeholder key, stable_pubkey_or_empty returns empty and
    # fetch_and_verify_stable_assets must hard-fail before touching the network.
    run chan_run "
        ados_fetch() { echo 'NETWORK SHOULD NOT BE HIT' >&2; return 1; }
        fetch_and_verify_stable_assets v0.40.4 '${CHAN_TMP}/assets' && echo RESULT_OK || echo RESULT_FAIL
    "
    chan_teardown
    [[ "$output" == *"RESULT_FAIL"* ]]
}

@test "stable channel REFUSES a tampered/unverifiable artifact (mocked verify)" {
    chan_setup
    # Real key embedded + downloads succeed, but ados_verify_artifact reports
    # failure (tamper / bad signature). The fetch helper must propagate the
    # refusal — stable is allowed to hard-fail on a bad signature.
    run chan_run "
        ADOS_STABLE_PUBKEY='RWQrealkeyrealkeyrealkeyrealkeyrealkeyrealkeyrealkeyAA'
        ados_fetch() { : > \"\$2\"; return 0; }
        ados_verify_artifact() { return 1; }
        fetch_and_verify_stable_asset https://example/base art.whl '${CHAN_TMP}/d' KEY && echo RESULT_OK || echo RESULT_FAIL
    "
    chan_teardown
    [[ "$output" == *"RESULT_FAIL"* ]]
}

@test "stable channel ACCEPTS a verified artifact (mocked verify ok)" {
    chan_setup
    run chan_run "
        ados_fetch() { : > \"\$2\"; return 0; }
        ados_verify_artifact() { return 0; }
        fetch_and_verify_stable_asset https://example/base art.whl '${CHAN_TMP}/d' KEY && echo RESULT_OK || echo RESULT_FAIL
    "
    chan_teardown
    [[ "$output" == *"RESULT_OK"* ]]
}

@test "install_agent_from_wheel records a wheel install in the pip log" {
    chan_setup
    chan_run "install_agent_from_wheel '/tmp/w.whl'"
    run cat "${CHAN_TMP}/pip.log"
    chan_teardown
    [[ "$output" == *"w.whl"* ]]
    [[ "$output" != *"git+"* ]]
}

@test "install_agent_from_wheel with extras installs the extras group" {
    chan_setup
    chan_run "install_agent_from_wheel '/tmp/w.whl' ground-station"
    run cat "${CHAN_TMP}/pip.log"
    chan_teardown
    [[ "$output" == *"w.whl[ground-station]"* ]]
}

@test "unpack_deploy_bundle extracts into destroot/repo and validates the tree" {
    chan_setup
    # Build a minimal bundle whose root dir is "repo" with data/systemd.
    STAGING="$(mktemp -d)"
    mkdir -p "${STAGING}/repo/data/systemd"
    : > "${STAGING}/repo/data/systemd/ados-supervisor.service"
    tar -czf "${CHAN_TMP}/bundle.tar.gz" -C "${STAGING}" repo
    rm -rf "${STAGING}"
    run chan_run "unpack_deploy_bundle '${CHAN_TMP}/bundle.tar.gz' '${CHAN_TMP}/dest' && [ -d '${CHAN_TMP}/dest/repo/data/systemd' ] && echo UNPACKED"
    chan_teardown
    [[ "$output" == *"UNPACKED"* ]]
}

@test "unpack_deploy_bundle fails on a bundle missing the systemd tree" {
    chan_setup
    STAGING="$(mktemp -d)"
    mkdir -p "${STAGING}/repo/scripts"
    : > "${STAGING}/repo/scripts/placeholder"
    tar -czf "${CHAN_TMP}/bad.tar.gz" -C "${STAGING}" repo
    rm -rf "${STAGING}"
    run chan_run "unpack_deploy_bundle '${CHAN_TMP}/bad.tar.gz' '${CHAN_TMP}/dest2' && echo UNPACKED || echo REFUSED"
    chan_teardown
    [[ "$output" == *"REFUSED"* ]]
}

@test "show_stable_key reports placeholder status while unprovisioned" {
    chan_setup
    run chan_run "show_stable_key"
    chan_teardown
    [[ "$output" == *"PLACEHOLDER"* ]]
}

# -----------------------------------------------------------------------------
# Broken-venv-pip self-recovery (ensure_venv_pip).
#
# The venv's pip can rot independently of the agent package (stale pip vendor
# tree, interrupted self-upgrade, Python minor bump). ensure_venv_pip probes
# the venv pip and self-heals: ensurepip first, then a full venv recreate +
# agent reinstall. These tests stub the venv python so the recovery ladder is
# observable without a real interpreter or network. find_python lives in
# 00-detect.sh (not sourced by orch_run), so each snippet stubs it.
# -----------------------------------------------------------------------------

@test "ensure_venv_pip is a no-op when venv pip already works" {
    orch_setup
    # The default orch_setup python shim succeeds for everything, so
    # `python -m pip --version` passes and the probe short-circuits. A
    # sentinel file proves ensurepip / recreate were never reached.
    run orch_run "
        find_python() { echo /usr/bin/python3; }
        ensure_venv_pip && echo RECOVER_OK || echo RECOVER_FAIL
        echo \"REQFAIL=[\${REQUIRED_FAILURES}]\"
    "
    orch_teardown
    [[ "$output" == *"RECOVER_OK"* ]]
    # No required failure recorded on a healthy pip.
    [[ "$output" == *"REQFAIL=[]"* ]]
}

@test "ensure_venv_pip repairs a broken pip via ensurepip (stage 1)" {
    orch_setup
    # python shim: `-m pip --version` fails until ensurepip has been run
    # (marked by a flag file); `-m ensurepip` drops the flag; afterwards
    # pip --version succeeds. This exercises the stage-1 in-place repair
    # without ever recreating the venv.
    cat > "${ORCH_TMP}/venv/bin/python" <<EOF
#!/usr/bin/env bash
FLAG="${ORCH_TMP}/ensurepip.done"
if [ "\$1" = "-m" ] && [ "\$2" = "ensurepip" ]; then
    : > "\${FLAG}"
    exit 0
fi
if [ "\$1" = "-m" ] && [ "\$2" = "pip" ] && [ "\$3" = "--version" ]; then
    [ -f "\${FLAG}" ] && exit 0 || exit 1
fi
exit 0
EOF
    chmod +x "${ORCH_TMP}/venv/bin/python"
    run orch_run "
        find_python() { echo /usr/bin/python3; }
        ensure_venv_pip && echo RECOVER_OK || echo RECOVER_FAIL
        # ensurepip must have been the recovery path (marker dropped by the
        # shim's -m ensurepip branch). Checked inside the snippet because
        # orch_teardown removes ORCH_TMP before the post-run assertions.
        [ -f '${ORCH_TMP}/ensurepip.done' ] && echo ENSUREPIP=YES || echo ENSUREPIP=NO
    "
    orch_teardown
    [[ "$output" == *"RECOVER_OK"* ]]
    [[ "$output" == *"ENSUREPIP=YES"* ]]
}

@test "ensure_venv_pip recreates the venv when ensurepip cannot repair pip (stage 2)" {
    orch_setup
    # python shim: pip --version and ensurepip both keep failing on the
    # ORIGINAL venv, forcing escalation to a full recreate. find_python
    # returns a stub interpreter whose `-m venv DIR` writes a NEW python
    # into DIR that reports a working pip. The recreate path must run and
    # leave a working pip behind. A marker proves `venv` was invoked.
    cat > "${ORCH_TMP}/venv/bin/python" <<'EOF'
#!/usr/bin/env bash
# Original venv: pip is permanently broken, ensurepip cannot fix it.
exit 1
EOF
    chmod +x "${ORCH_TMP}/venv/bin/python"
    cat > "${ORCH_BIN}/fakepy" <<EOF
#!/usr/bin/env bash
# Stub interpreter used to recreate the venv. \`-m venv DIR\` rebuilds the
# venv with a healthy python; anything else is a no-op success.
if [ "\$1" = "-m" ] && [ "\$2" = "venv" ]; then
    DIR="\${@: -1}"
    : > "${ORCH_TMP}/recreated"
    mkdir -p "\${DIR}/bin"
    cat > "\${DIR}/bin/python" <<'PYEOF'
#!/usr/bin/env bash
exit 0
PYEOF
    chmod +x "\${DIR}/bin/python"
    exit 0
fi
exit 0
EOF
    chmod +x "${ORCH_BIN}/fakepy"
    run orch_run "
        find_python() { echo '${ORCH_BIN}/fakepy'; }
        reinstall() { : > '${ORCH_TMP}/reinstalled'; }
        ensure_venv_pip reinstall && echo RECOVER_OK || echo RECOVER_FAIL
        # Emit the marker state into the captured output: orch_teardown rm's
        # ORCH_TMP before the post-run assertions, so file checks must be made
        # inside the snippet, not after teardown.
        [ -f '${ORCH_TMP}/recreated' ] && echo RECREATED=YES || echo RECREATED=NO
        [ -f '${ORCH_TMP}/reinstalled' ] && echo REINSTALLED=YES || echo REINSTALLED=NO
    "
    orch_teardown
    [[ "$output" == *"RECOVER_OK"* ]]
    [[ "$output" == *"RECREATED=YES"* ]]
    [[ "$output" == *"REINSTALLED=YES"* ]]
}

@test "ensure_venv_pip records a REQUIRED failure when recreate cannot recover pip" {
    orch_setup
    # Original venv python broken; recreate produces a venv whose python is
    # ALSO broken (simulating a fundamentally broken interpreter). The
    # function must give up, record a required failure, and return non-zero.
    cat > "${ORCH_TMP}/venv/bin/python" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
    chmod +x "${ORCH_TMP}/venv/bin/python"
    cat > "${ORCH_BIN}/fakepy" <<EOF
#!/usr/bin/env bash
if [ "\$1" = "-m" ] && [ "\$2" = "venv" ]; then
    DIR="\${@: -1}"
    mkdir -p "\${DIR}/bin"
    cat > "\${DIR}/bin/python" <<'PYEOF'
#!/usr/bin/env bash
exit 1
PYEOF
    chmod +x "\${DIR}/bin/python"
    exit 0
fi
exit 0
EOF
    chmod +x "${ORCH_BIN}/fakepy"
    run orch_run "
        find_python() { echo '${ORCH_BIN}/fakepy'; }
        ensure_venv_pip && echo RECOVER_OK || echo RECOVER_FAIL
        echo \"REQFAIL=[\${REQUIRED_FAILURES}]\"
    "
    orch_teardown
    [[ "$output" == *"RECOVER_FAIL"* ]]
    [[ "$output" == *"venv-pip"* ]]
}

@test "ensure_venv_pip records a REQUIRED failure when no interpreter is available to recreate" {
    orch_setup
    cat > "${ORCH_TMP}/venv/bin/python" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
    chmod +x "${ORCH_TMP}/venv/bin/python"
    run orch_run "
        find_python() { echo ''; }
        ensure_venv_pip && echo RECOVER_OK || echo RECOVER_FAIL
        echo \"REQFAIL=[\${REQUIRED_FAILURES}]\"
    "
    orch_teardown
    [[ "$output" == *"RECOVER_FAIL"* ]]
    [[ "$output" == *"venv-pip"* ]]
}

# -----------------------------------------------------------------------------
# Mid-step abort fallback (install_failure_trap + write-once guard).
#
# A hard failure inside the install body under `set -e` kills the script
# before run_health_gate, so no result file is written. install_failure_trap
# is the EXIT-trap fallback that writes a "failed" result. It must:
#   - write a failed result when the body exits non-zero and nothing has been
#     written yet, recording the current step;
#   - do NOTHING on a clean (exit 0) body;
#   - NOT clobber a result the health gate already wrote (write-once via the
#     ADOS_RESULT_WRITTEN guard).
# -----------------------------------------------------------------------------

@test "install_failure_trap writes a failed result on a non-zero body exit" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    chmod +x "${ORCH_BIN}/uname"
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 1.0.0; }
        ADOS_CURRENT_STEP=upgrade-pip-package
        install_failure_trap 1
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [ "$status" -eq 0 ]
    [[ "$output" == *'"status": "failed"'* ]]
    [[ "$output" == *"upgrade-pip-package"* ]]
}

@test "install_failure_trap is a no-op on a clean (exit 0) body" {
    orch_setup
    cat > "${ORCH_BIN}/uname" <<'EOF'
#!/usr/bin/env bash
[ "$1" = "-s" ] && { echo Linux; exit 0; }
exec /usr/bin/uname "$@"
EOF
    chmod +x "${ORCH_BIN}/uname"
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 1.0.0; }
        install_failure_trap 0
        [ -f '${ORCH_TMP}/state/install-result.json' ] && echo WROTE || echo NO_FILE
    "
    orch_teardown
    [[ "$output" == *"NO_FILE"* ]]
}

@test "install_failure_trap does NOT clobber a result the health gate already wrote (write-once)" {
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
        # Normal path writes 'ok' and flips ADOS_RESULT_WRITTEN.
        run_health_gate && echo GATE_OK || echo GATE_FAIL
        # The trap then fires (e.g. a later exit); it must NOT overwrite the ok.
        install_failure_trap 1
        echo '---'
        /bin/cat '${ORCH_TMP}/state/install-result.json'
    "
    orch_teardown
    [[ "$output" == *"GATE_OK"* ]]
    [[ "$output" == *'"status": "ok"'* ]]
    [[ "$output" != *'"status": "failed"'* ]]
}

@test "write_install_result flips the ADOS_RESULT_WRITTEN guard" {
    orch_setup
    run orch_run "
        ADOS_PROFILE=drone
        get_installed_version() { echo 1.0.0; }
        echo \"BEFORE=\${ADOS_RESULT_WRITTEN}\"
        write_install_result ok
        echo \"AFTER=\${ADOS_RESULT_WRITTEN}\"
    "
    orch_teardown
    [[ "$output" == *"BEFORE=0"* ]]
    [[ "$output" == *"AFTER=1"* ]]
}

# -----------------------------------------------------------------------------
# Source-shape regressions for the inline-install wiring.
# -----------------------------------------------------------------------------

@test "13-main upgrade path self-heals the venv pip before the package install" {
    # ensure_venv_pip must be called on the upgrade path before the
    # 'Upgrading pip package...' info line so a broken pip self-heals instead
    # of aborting the upgrade.
    heal_line="$(grep -nE 'ensure_venv_pip _upgrade_reinstall_agent' "${INSTALL_D}/13-main.sh" | head -1 | cut -d: -f1)"
    upg_line="$(grep -nE 'Upgrading pip package' "${INSTALL_D}/13-main.sh" | head -1 | cut -d: -f1)"
    [ -n "$heal_line" ]
    [ -n "$upg_line" ]
    [ "$heal_line" -lt "$upg_line" ]
}

@test "dispatcher wires the install-body EXIT trap before main_install_flow" {
    trap_line="$(grep -nE 'trap .+_install_body_exit.+ EXIT' "${DISPATCHER}" | head -1 | cut -d: -f1)"
    flow_line="$(grep -nE '^main_install_flow[[:space:]]*$' "${DISPATCHER}" | head -1 | cut -d: -f1)"
    [ -n "$trap_line" ]
    [ -n "$flow_line" ]
    # The failed-result fallback trap must be wired before the inline run so a
    # mid-step abort under set -e still lands an install-result.json.
    [ "$trap_line" -lt "$flow_line" ]
}

@test "install-body EXIT trap preserves the bootstrap-dir cleanup" {
    # The single-slot EXIT trap replaces the bootstrap cleanup; the handler
    # must fold that cleanup back in so a curl-pipe clone is still removed.
    run grep -nE 'ADOS_BOOTSTRAP_DIR' "${DISPATCHER}"
    [ "$status" -eq 0 ]
    # The _install_body_exit handler references ADOS_BOOTSTRAP_DIR for cleanup.
    run bash -c "awk '/_install_body_exit\\(\\)/,/^}/' '${DISPATCHER}' | grep -c 'ADOS_BOOTSTRAP_DIR'"
    [ "$status" -eq 0 ]
    [ "$output" -ge 1 ]
}

# -----------------------------------------------------------------------------
# Display overlay: auto mode must never apply a boot-critical SPI-LCD overlay.
#
# A headless rig with no panel attached must boot. Auto-selecting and applying
# an SPI-LCD overlay rewrites the board's only boot config + queues a
# framebuffer driver every boot, which on an extlinux/u-boot board (Rock 5C
# and similar) leaves the system unbootable with no power-cycle recovery.
# These tests drive scripts/drivers/install-display-overlay.sh with its write
# paths redirected to a temp tree (the ADOS_*_DIR overrides) so they can assert
# exactly which files each path touches without root.
# -----------------------------------------------------------------------------

overlay_setup() {
    OVL_TMP="$(mktemp -d)"
    OVL_SCRIPT="${REPO_ROOT}/scripts/drivers/install-display-overlay.sh"
    [ -x "${OVL_SCRIPT}" ] || {
        echo "missing overlay installer: ${OVL_SCRIPT}" >&2
        return 1
    }
    mkdir -p "${OVL_TMP}/boot" "${OVL_TMP}/etc-ados" "${OVL_TMP}/modules-load" "${OVL_TMP}/bin"
}

overlay_teardown() {
    [ -n "${OVL_TMP:-}" ] && rm -rf "${OVL_TMP}"
}

# Run the overlay installer with every write path redirected into OVL_TMP.
# $@ = installer args. Extra PATH (mock tools) comes via OVL_EXTRA_PATH.
overlay_run() {
    PATH="${OVL_EXTRA_PATH:-}${OVL_EXTRA_PATH:+:}${PATH}" \
    ADOS_OVERLAY_ALLOW_NONROOT=1 \
    ADOS_BOOT_DIR="${OVL_TMP}/boot" \
    ADOS_ETC_DIR="${OVL_TMP}/etc-ados" \
    ADOS_MODULES_LOAD_DIR="${OVL_TMP}/modules-load" \
    ADOS_DISPLAY_CONF="${OVL_TMP}/etc-ados/display.conf" \
    ADOS_MODULES_LOAD_FILE="${OVL_TMP}/modules-load/ados-display.conf" \
    bash "${OVL_SCRIPT}" "$@"
}

@test "overlay --display auto is a no-op on a board whose only display is an SPI-LCD" {
    overlay_setup
    # rock-5c-lite's HAL YAML lists exactly one display: waveshare35a, type
    # spi-lcd. Auto must refuse to provision it: return 0, write display_id=none,
    # touch NOTHING under /boot and NOTHING under /etc/modules-load.d.
    run overlay_run --board rock-5c-lite --display auto
    [ "$status" -eq 0 ]
    [[ "$output" == *"auto mode will not provision it"* ]]

    # No boot-config edit.
    boot_files="$(find "${OVL_TMP}/boot" -type f 2>/dev/null | wc -l | tr -d ' ')"
    # No modules-load.d drop-in.
    ml_files="$(find "${OVL_TMP}/modules-load" -type f 2>/dev/null | wc -l | tr -d ' ')"
    # display.conf records "none".
    conf_id="$(grep -E '^display_id=' "${OVL_TMP}/etc-ados/display.conf" 2>/dev/null || true)"
    overlay_teardown
    [ "$boot_files" = "0" ]
    [ "$ml_files" = "0" ]
    [ "${conf_id}" = "display_id=none" ]
}

@test "overlay auto-skip writes no extlinux.conf and runs no u-boot-update" {
    overlay_setup
    # Belt-and-suspenders: even with a managed.list + extlinux.conf present
    # (the Radxa Bookworm shape that the brick incident hit), auto mode must
    # not invoke u-boot-update or modify the boot config.
    mkdir -p "${OVL_TMP}/boot/dtbo" "${OVL_TMP}/boot/extlinux"
    : > "${OVL_TMP}/boot/dtbo/managed.list"
    printf 'LABEL ados\n  append root=/dev/mmcblk0p2 rw\n' > "${OVL_TMP}/boot/extlinux/extlinux.conf"
    extlinux_before="$(cat "${OVL_TMP}/boot/extlinux/extlinux.conf")"
    # A u-boot-update mock that screams if it ever runs.
    cat > "${OVL_TMP}/bin/u-boot-update" <<'EOF'
#!/usr/bin/env bash
echo "U_BOOT_UPDATE_RAN_UNEXPECTEDLY"
exit 0
EOF
    chmod +x "${OVL_TMP}/bin/u-boot-update"
    OVL_EXTRA_PATH="${OVL_TMP}/bin"
    run overlay_run --board rock-5c-lite --display auto
    extlinux_after="$(cat "${OVL_TMP}/boot/extlinux/extlinux.conf")"
    managed_after="$(cat "${OVL_TMP}/boot/dtbo/managed.list")"
    overlay_teardown
    [ "$status" -eq 0 ]
    [[ "$output" != *"U_BOOT_UPDATE_RAN_UNEXPECTEDLY"* ]]
    [ "${extlinux_before}" = "${extlinux_after}" ]
    [ -z "${managed_after}" ]
}

@test "overlay explicit --display waveshare35a still attempts to apply (no auto-skip)" {
    overlay_setup
    # The explicit opt-in path (operator / setup webapp passes --display <id>)
    # MUST proceed into the per-board apply branch — the auto-skip is for the
    # auto path only. We mock dtc + cpp + u-boot-update so the apply branch can
    # run on a CI host without real kernel headers, and provide the Radxa
    # managed.list shape so activation has a real branch to take. We assert the
    # apply branch was entered (its marker log line) and that the auto-skip
    # message did NOT appear. We do NOT require full success — only that the
    # explicit path is not short-circuited to "none".
    mkdir -p "${OVL_TMP}/boot/dtbo" "${OVL_TMP}/boot/extlinux"
    : > "${OVL_TMP}/boot/dtbo/managed.list"
    printf 'LABEL ados\n  append root=/dev/mmcblk0p2 rw\n' > "${OVL_TMP}/boot/extlinux/extlinux.conf"
    cat > "${OVL_TMP}/bin/dtc" <<'EOF'
#!/usr/bin/env bash
out=""; while [ $# -gt 0 ]; do [ "$1" = "-o" ] && out="$2"; shift; done
[ -n "$out" ] && printf 'DTBO' > "$out"; exit 0
EOF
    cat > "${OVL_TMP}/bin/cpp" <<'EOF'
#!/usr/bin/env bash
out=""; while [ $# -gt 0 ]; do [ "$1" = "-o" ] && out="$2"; shift; done
[ -n "$out" ] && printf '/dts-v1/;\n' > "$out"; exit 0
EOF
    cat > "${OVL_TMP}/bin/u-boot-update" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "${OVL_TMP}/bin/dtc" "${OVL_TMP}/bin/cpp" "${OVL_TMP}/bin/u-boot-update"
    OVL_EXTRA_PATH="${OVL_TMP}/bin"
    run overlay_run --board rock-5c-lite --display waveshare35a
    overlay_teardown
    # The per-board apply branch logs this before compiling the vendored DTS.
    [[ "$output" == *"Installing vendored DTS"* ]]
    # The auto-skip must NOT have fired on an explicit request.
    [[ "$output" != *"auto mode will not provision it"* ]]
}

@test "overlay explicit --display none is an immediate no-op" {
    overlay_setup
    run overlay_run --board rock-5c-lite --display none
    boot_files="$(find "${OVL_TMP}/boot" -type f 2>/dev/null | wc -l | tr -d ' ')"
    ml_files="$(find "${OVL_TMP}/modules-load" -type f 2>/dev/null | wc -l | tr -d ' ')"
    overlay_teardown
    [ "$status" -eq 0 ]
    [ "$boot_files" = "0" ]
    [ "$ml_files" = "0" ]
}

# -----------------------------------------------------------------------------
# install_display_driver (03-kernel.sh): the per-profile default display.
#
# A drone / lite rig must default the display to "none" (a headless flight rig
# has no panel, and the auto-pickable display is a boot-critical SPI-LCD). A
# ground station defaults to "auto". An explicit ADOS_DISPLAY always wins.
# We stub the overlay script so the function's chosen --display value is
# captured, and stub the script-discovery so the function finds our stub.
# -----------------------------------------------------------------------------

idd_run() {
    # $1 = ADOS_PROFILE value, $2 = ADOS_DISPLAY value (may be empty).
    # Echoes the --display argument install_display_driver passed to the
    # overlay script.
    local profile="$1" display="${2:-}"
    local tmp; tmp="$(mktemp -d)"
    mkdir -p "${tmp}/drivers"
    # Stub overlay installer: record the --display arg, succeed.
    cat > "${tmp}/drivers/install-display-overlay.sh" <<EOF
#!/usr/bin/env bash
while [ \$# -gt 0 ]; do
    case "\$1" in
        --display) echo "CAPTURED_DISPLAY=\$2" > "${tmp}/captured"; shift 2 ;;
        *) shift ;;
    esac
done
exit 0
EOF
    chmod +x "${tmp}/drivers/install-display-overlay.sh"
    bash -c "
        set -u
        info()  { :; }
        warn()  { :; }
        error() { :; }
        source '${INSTALL_D}/lib.sh'
        source '${INSTALL_D}/03-kernel.sh'
        # Point script discovery at the stub via FRESH_REPO_DIR/repo layout.
        export FRESH_REPO_DIR='${tmp}'
        mkdir -p '${tmp}/repo/scripts'
        cp -r '${tmp}/drivers' '${tmp}/repo/scripts/drivers'
        export ADOS_PROFILE='${profile}'
        $( [ -n "${display}" ] && echo "export ADOS_DISPLAY='${display}'" )
        install_display_driver >/dev/null 2>&1 || true
        cat '${tmp}/captured' 2>/dev/null || echo 'CAPTURED_DISPLAY=<none-run>'
    "
    rm -rf "${tmp}"
}

@test "install_display_driver defaults to none on the drone profile" {
    output="$(idd_run drone)"
    [[ "$output" == *"CAPTURED_DISPLAY=none"* ]]
}

@test "install_display_driver defaults to none on the lite profile" {
    output="$(idd_run lite-rs)"
    [[ "$output" == *"CAPTURED_DISPLAY=none"* ]]
}

@test "install_display_driver defaults to auto on the ground-station profile" {
    output="$(idd_run ground_station)"
    [[ "$output" == *"CAPTURED_DISPLAY=auto"* ]]
}

@test "install_display_driver honors an explicit ADOS_DISPLAY over the profile default" {
    # Operator opt-in must win even on a drone profile.
    output="$(idd_run drone waveshare35a)"
    [[ "$output" == *"CAPTURED_DISPLAY=waveshare35a"* ]]
}
