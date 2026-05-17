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
                 11-artifacts 12-output; do
            source '${INSTALL_D}/'\$m.sh
        done
        echo OK
    "
    [ "$status" -eq 0 ]
    [[ "$output" == *"OK"* ]]
}

@test "all 37 spec-mapped functions resolve after sourcing modules" {
    run bash -c "
        set -e
        source '${INSTALL_D}/lib.sh'
        for m in 00-detect 01-state 02-deps 03-kernel 04-dkms 05-mesh \
                 06-radio 07-systemd 08-plugin 09-config 10-network \
                 11-artifacts 12-output; do
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
            wait_for_api_ready print_pairing_code print_hardware_summary print_status; do
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
    run grep -E '^export[[:space:]]+(FRESH_REPO_DIR|SYSTEMD_SRC_DIR)' "${DISPATCHER}"
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
