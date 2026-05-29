#!/usr/bin/env bash
# =============================================================================
# Main install flow — wraps the upgrade fast-path + fresh-install body that
# used to live inline at the bottom of install.sh. Sourced last by the
# dispatcher so the function is defined before the dispatcher's final
# main_install_flow call.
#
# All state inputs (PAIR_CODE, DO_UPGRADE, FRESH_REPO_DIR, BRANCH_NAME,
# DRONE_NAME, DO_FORCE, ADOS_PROFILE) are exported by the dispatcher
# before the function is invoked; bash function scoping makes them
# accessible without explicit parameters.
# =============================================================================

main_install_flow() {
    # Mandatory entry log. If the install log is empty after a run, that
    # is itself a signal that the dispatcher never reached this function
    # (sourcing failed, sudo bailed, redirect target wasn't writable).
    info "ADOS Drone Agent install.sh starting"
    ADOS_INSTALL_START="$(date +%s)"
    export ADOS_INSTALL_START
    echo ""
    echo -e "${BOLD}=== ADOS Drone Agent Installer ===${NC}"
    echo ""

    OS_TYPE=$(detect_os)
    ARCH=$(detect_arch)
    info "Platform: ${OS_TYPE} ${ARCH}"

    # ─── macOS Dev Mode ─────────────────────────────────────────────────────────

    if [ "$OS_TYPE" = "darwin" ]; then
        info "macOS detected. Installing in dev mode."
        echo ""

        PYTHON=$(find_python)
        if [ -z "$PYTHON" ]; then
            error "Python 3.11+ required. Install with: brew install python@3.12"
            exit 1
        fi
        info "Python: ${PYTHON} ($(${PYTHON} --version 2>&1 | awk '{print $2}'))"

        # Install using uv > pipx > pip (in order of preference)
        if command -v uv &>/dev/null; then
            info "Installing with uv..."
            uv tool install "git+${REPO_URL}"
        elif command -v pipx &>/dev/null; then
            info "Installing with pipx..."
            pipx install "git+${REPO_URL}"
        else
            info "Installing with pip..."
            "$PYTHON" -m pip install --user "git+${REPO_URL}"
        fi

        echo ""
        info "Installation complete (dev mode)."
        echo ""
        echo "  Run:    ados demo         # simulated drone telemetry"
        echo "          ados tui          # TUI dashboard"
        echo "          ados diag         # system diagnostics"
        echo "          ados version      # check version"
        echo ""
        echo "  No systemd on macOS. Use 'ados start' to run manually."
        exit 0
    fi

    # ─── Linux Production Mode ──────────────────────────────────────────────────

    # Must be root
    if [ "$(id -u)" -ne 0 ]; then
        error "Run as root: sudo ./install.sh"
        exit 1
    fi

    # Print detected OS
    if [ -f /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        info "OS: ${PRETTY_NAME:-${OS_TYPE}}"
    fi

    # Validate supported OS families
    case "$OS_TYPE" in
        raspbian|ubuntu|armbian|debian)
            info "Supported OS detected." ;;
        *)
            warn "Untested OS '${OS_TYPE}'. Proceeding anyway, but things may break." ;;
    esac

    # Validate architecture
    case "$ARCH" in
        aarch64|armhf|x86_64)
            info "Architecture: ${ARCH}" ;;
        *)
            warn "Unexpected architecture '${ARCH}'. Proceeding." ;;
    esac

    # ─── Stale-state auto-purge ──────────────────────────────────────────────────
    #
    # Residue from an incomplete prior uninstall (unit files surviving past
    # venv removal, orphan dropin dirs) used to silently block a fresh
    # install because some downstream step would trip on the leftovers
    # without writing anything observable to the log. purge_ados_artifacts
    # runs the same cleanup do_uninstall uses but does not call exit, so
    # the install continues after the sweep. Running it on a clean system
    # is a no-op.
    if detect_stale_state; then
        warn "Detected residue from an incomplete prior install or uninstall."
        warn "Auto-purging stale systemd units, dropins, and state before fresh install."
        purge_ados_artifacts
        info "Auto-purge complete. Continuing with fresh install."
    fi

    # Bootstrap the long-lived state dirs once, BEFORE any unit deployment
    # so ReadWritePaths=/var/lib/ados in ados-api.service can never see a
    # missing target. setup_state_dirs is idempotent.
    setup_state_dirs

    # ─── Fast Path: Pair-only (already installed, --pair/positional code) ────────

    if is_installed && [ -n "$PAIR_CODE" ] && ! $DO_FORCE; then
        info "Agent already installed ($(get_installed_version)). Fast path: updating pairing code only."
        mkdir -p "${CONFIG_DIR}"
        write_pairing "$PAIR_CODE"
        systemctl restart "${SERVICE_NAME}" 2>/dev/null || true
        print_pairing_code
        info "Done. Service restarted with new pairing code."
        exit 0
    fi

    # ─── Already installed, no flags: complete? short-circuit. incomplete? resume ─
    #
    # The old gate short-circuited the moment the `ados` binary existed,
    # even when the install never finished (units never deployed, supervisor
    # never enabled). A box left half-installed by a dropped SSH session
    # would then report "already installed" on the next plain re-run and
    # silently skip the missing steps forever. Now we probe COMPLETENESS:
    # global command + importable venv + supervisor unit + every
    # profile-expected unit enabled. Complete => keep the friendly
    # already-installed message. Incomplete => fall through to the full
    # install body, which is idempotent and finishes the missing work
    # (a resume). No --force required to recover a half-install.

    if is_installed && ! $DO_FORCE && ! $DO_UPGRADE; then
        # Resolve the profile so the completeness probe checks the right
        # unit set. resolve_profile reads the persisted profile.conf.
        ADOS_PROFILE="$(resolve_profile)"
        export ADOS_PROFILE

        if is_install_complete "${ADOS_PROFILE}"; then
            local_ver=$(get_installed_version)

            # Ensure global symlinks exist (fixes installs from before symlink support)
            install_global_symlinks

            echo ""
            info "ADOS Drone Agent already installed (v${local_ver})."
            echo ""
            echo "  Status:    sudo systemctl status ${SERVICE_NAME}"
            echo "  CLI:       ados status"
            echo ""
            echo "  Re-run with:"
            echo "    --upgrade    Update to latest version (skip apt, skip venv rebuild)"
            echo "    --force      Full reinstall from scratch"
            echo "    --pair CODE  Update pairing code only (<5s)"
            echo "    CODE         Same as --pair CODE (positional)"
            echo ""
            print_pairing_code
            exit 0
        fi

        warn "Agent present but install is INCOMPLETE (missing: ${INSTALL_MISSING})."
        warn "Resuming install to finish the missing steps. No --force needed."
        info "Completed checkpoints: $(list_completed_checkpoints)"
        # Fall through to the full install body below. Every step is
        # idempotent; checkpoints make the finished ones fast no-ops.
    fi

    # ─── Upgrade Path (skip apt, skip venv creation) ────────────────────────────

    if is_installed && $DO_UPGRADE && ! $DO_FORCE; then
        # ADOS_CURRENT_STEP tracks the install-body phase so the dispatcher's
        # EXIT trap (install_failure_trap in 14-orchestration.sh) can attribute
        # a mid-step abort in the failed install-result.json. Exported so the
        # trap, which runs in this same process, reads the latest value.
        export ADOS_CURRENT_STEP="upgrade"
        # Four operator-facing stages for an upgrade (fewer than a fresh
        # install: apt + venv are already in place). These feed the staging
        # helpers sourced from lib.sh (hence the cross-file disable).
        # shellcheck disable=SC2034
        ADOS_STEP_NUM=0 ADOS_STEP_TOTAL=4
        info "Upgrading ADOS Drone Agent..."
        local_ver=$(get_installed_version)
        info "Current version: ${local_ver}"

        # Resolve the agent profile early on the upgrade path. Without this
        # line ADOS_PROFILE stayed empty for the rest of the block and every
        # `${ADOS_PROFILE:-drone}` check defaulted to "drone", which meant
        # ground-station upgrades silently skipped the GS-only steps and the
        # cross-profile teardown ran the wrong direction. resolve_profile
        # reads --profile flag first, then /etc/ados/profile.conf — both of
        # which are stable across upgrades on a previously-installed rig.
        ADOS_PROFILE="$(resolve_profile)"
        info "Detected profile: ${ADOS_PROFILE}"

        # Rigs first installed before this revision still have the Debian
        # dnsmasq.service and hostapd.service enabled; mask them on every
        # ground-station upgrade so the standalone units cannot keep racing
        # the ADOS-owned ports. No-op on drone profile and no-op on rigs
        # where the units are already masked.
        if [ "${ADOS_PROFILE}" = "ground_station" ]; then
            mask_conflicting_standalone_services
        fi

        # Ensure system deps are present. The upgrade path skips the full
        # install_system_deps to keep upgrades fast, so we only top up the
        # packages that earlier installs may have missed. Includes the
        # wfb-ng runtime Python deps (twisted et al.) so wfb-server can
        # start the bind protocol on rigs first installed before v0.16.4.
        ados_stage_begin "Checking dependencies"
        info "Checking system dependencies..."
        for pkg in \
            ffmpeg v4l-utils avahi-daemon \
            gstreamer1.0-tools gstreamer1.0-rtsp \
            python3-twisted python3-serial python3-jinja2 \
            python3-msgpack python3-pyroute2 socat; do
            if ! dpkg -s "$pkg" &>/dev/null; then
                info "Installing missing system dependency: ${pkg}"
                apt-get install -y -qq "$pkg" 2>/dev/null || true
            fi
        done
        ados_stage_end

        ados_stage_begin "Installing agent software"
        # Record the active release channel for the journal.
        print_channel_banner

        # Acquire the install source tree + new agent package per channel.
        #
        #   stable — resolve the tag (pin via --version or latest v*), fetch +
        #            verify the signed wheel and signed deploy-bundle, unpack
        #            the bundle into tmp_repo (same repo/ layout the clone
        #            produces), then pip-install the verified wheel. Config is
        #            preserved (generate_default_config is skip-if-exists and
        #            is not re-run on the upgrade path). Any miss hard-fails.
        #
        #   edge   — clone the repo (honouring --branch) and pip-install from
        #            source. Unchanged historical behaviour.
        tmp_repo="$(mktemp -d)"
        upgrade_wheel=""
        if is_stable_channel; then
            local up_tag
            up_tag="$(resolve_stable_tag)" || {
                error "stable channel: could not resolve a release tag for upgrade."
                rm -rf "${tmp_repo}"
                record_failure "stable-tag-resolve" required
                run_health_gate || true
                exit 1
            }
            info "Stable channel: pinning upgrade to release ${up_tag}."
            if ! fetch_and_verify_stable_assets "${up_tag}" "${tmp_repo}/assets"; then
                error "stable channel: failed to fetch or verify upgrade assets for ${up_tag}."
                rm -rf "${tmp_repo}"
                record_failure "stable-assets" required
                run_health_gate || true
                exit 1
            fi
            if ! unpack_deploy_bundle "${STABLE_BUNDLE_PATH}" "${tmp_repo}"; then
                error "stable channel: failed to unpack the verified upgrade deploy-bundle."
                rm -rf "${tmp_repo}"
                record_failure "stable-bundle-unpack" required
                run_health_gate || true
                exit 1
            fi
            upgrade_wheel="${STABLE_WHEEL_PATH}"
        else
            info "Fetching latest source..."
            # Attribute a clone failure to this step: git_clone_retry returns
            # non-zero after its retries, which aborts under set -e and fires
            # the EXIT trap, which records ADOS_CURRENT_STEP as the failed step.
            ADOS_CURRENT_STEP="clone-source"
            # honor --branch for feature-branch installs. Bounded retry so a
            # transient network blip does not abort the upgrade.
            if [ -n "$BRANCH_NAME" ]; then
                info "Using branch: ${BRANCH_NAME}"
                git_clone_retry "${tmp_repo}/repo" "${BRANCH_NAME}"
            else
                git_clone_retry "${tmp_repo}/repo"
            fi
        fi

        # Migrate older venvs that were created without
        # --system-site-packages so the agent can `import gi` (PyGObject)
        # for the LCD video page's gstreamer pipeline. python3-gi is an
        # apt-only package; pip can't install it. Idempotent: if the flag
        # is already true, sed leaves the file unchanged.
        if [ -f "${VENV_DIR}/pyvenv.cfg" ]; then
            if grep -q "^include-system-site-packages = false" "${VENV_DIR}/pyvenv.cfg"; then
                info "Flipping venv to include-system-site-packages=true (gi/gstreamer access)"
                sed -i 's|^include-system-site-packages = false|include-system-site-packages = true|' \
                    "${VENV_DIR}/pyvenv.cfg"
            fi
        fi

        # The upgrade leans on the venv's pip for the package install below.
        # A venv pip can rot independently of the agent (a stale pip vendor
        # tree, an interrupted self-upgrade, a Python minor bump) and crash on
        # `python -m pip --version`; left unhandled that aborts the upgrade
        # under set -e and the box silently stays on the old version. Probe +
        # self-heal first. The reinstaller closure reinstalls the agent package
        # into the rebuilt venv when a recreate was needed — channel-correct
        # (verified wheel on stable, cloned source on edge). A broken pip must
        # never silently no-op the upgrade.
        ADOS_CURRENT_STEP="upgrade-pip-package"
        _upgrade_reinstall_agent() {
            if is_stable_channel; then
                install_agent_from_wheel "${upgrade_wheel}"
            else
                ensure_build_toolchain
                "${VENV_DIR}/bin/pip" install --upgrade "${tmp_repo}/repo" --quiet
            fi
        }
        if ! ensure_venv_pip _upgrade_reinstall_agent; then
            error "venv pip could not be recovered; aborting upgrade before the package install."
            rm -rf "${tmp_repo}"
            run_health_gate || true
            unset -f _upgrade_reinstall_agent
            exit 1
        fi

        # Upgrade the pip package. On stable from the verified wheel; on edge
        # from the cloned source. config.yaml + pairing state are untouched.
        # When ensure_venv_pip recreated the venv it already reinstalled the
        # agent via the same closure, so this is idempotent (pip --upgrade is a
        # no-op when the wheel/source is already the installed version).
        info "Upgrading pip package..."
        _upgrade_reinstall_agent
        unset -f _upgrade_reinstall_agent

        new_ver=$(get_installed_version)
        if [ "$local_ver" = "$new_ver" ]; then
            info "Already on latest version (${new_ver})."
        else
            info "Upgraded: ${local_ver} -> ${new_ver}"
        fi
        ados_stage_end

        ados_stage_begin "Updating services and configuration"
        # Ensure mediamtx is installed
        install_mediamtx

        # Update systemd service files from cloned repo
        if [ -d "${tmp_repo}/repo/data/systemd" ]; then
            SYSTEMD_SRC_DIR="${tmp_repo}/repo/data/systemd"
        fi
        install_systemd_service

        # install_systemd_service restarts ados-supervisor, and the
        # ground-station child units (hostapd, dnsmasq-gs, wfb-rx, etc.)
        # carry PartOf=ados-supervisor.service so they stop on that
        # restart. Nothing in the rest of the upgrade block starts them
        # again — the fresh-install path reaches enable_ground_station_units
        # via the main install body, but --upgrade never did. Mirror the
        # call here so the AP comes back without an operator running
        # systemctl by hand.
        if [ "${ADOS_PROFILE}" = "ground_station" ]; then
            enable_ground_station_units
            # Reconcile native-consolidator masks against the cutover flags.
            # Ground-station only (cross-profile ados-wifi-client never masked
            # on drone).
            reconcile_rust_cutover_masks
        fi

        # Orphan AP IP cleanup: a previously-active setup-webapp captive
        # portal can leave 192.168.4.1/24 on wlan0 even after the AP is
        # torn down. Avahi then publishes that address via mDNS and the
        # browser may try it as a candidate for the agent hostname,
        # producing a connection timeout. Drop the address when no AP
        # service is currently active.
        if ip -4 addr show wlan0 2>/dev/null | grep -q "inet 192\.168\.4\.1/"; then
            if ! systemctl is-active --quiet hostapd 2>/dev/null \
                && ! systemctl is-active --quiet ados-setup-ap 2>/dev/null \
                && ! systemctl is-active --quiet ados-captive-portal 2>/dev/null; then
                info "Removing orphan AP address 192.168.4.1/24 from wlan0"
                ip addr del 192.168.4.1/24 dev wlan0 2>/dev/null || true
            fi
        fi

        # LCD overlay installer needs the cloned scripts + DTS sources,
        # so it runs before the temp-repo cleanup. Runs on every profile;
        # install_display_driver picks a profile-aware default display
        # (none on a drone, auto on a ground station) unless
        # the operator forced one with ADOS_DISPLAY. A headless drone
        # never auto-provisions a boot-critical SPI-LCD overlay, so this
        # is a fast no-op there.
        #
        # Fail-fast on overlay install failure during the upgrade flow.
        # The installer regenerates /boot/extlinux/extlinux.conf on
        # Radxa boards via u-boot-update — if that step fails we bail
        # the entire upgrade rather than continue with a potentially
        # broken bootloader and let the operator power-cycle into a
        # brick. The installer itself snapshots extlinux.conf and
        # restores on failure, so by the time control returns here the
        # box should still be bootable; this guard is the second line
        # of defense.
        if ! FRESH_REPO_DIR="${tmp_repo}" install_display_driver; then
            error "install_display_driver failed during upgrade. Aborting before any more boot-critical changes."
            error "Inspect /boot/extlinux/extlinux.conf and /boot/dtbo/managed.list before power-cycling."
            rm -rf "${tmp_repo}"
            exit 1
        fi
        ados_stage_end

        ados_stage_begin "Updating radios and finishing up"
        # Persist driver scripts + overlay sources to /opt/ados/source/ so the
        # wizard's display step (and any future CLI re-runs) can find them
        # without a fresh git clone.
        FRESH_REPO_DIR="${tmp_repo}" persist_repo_artifacts

        # wfb-ng userspace from the vendored source — must run BEFORE the
        # temp-repo cleanup so vendor/wfb-ng/ is still on disk. Build deps
        # are best-effort; the function bails clean if anything is missing.
        # The required wfb-ng build deps are installed as one group. The
        # gstreamer -dev headers (only needed for the optional wfb_rtsp
        # target) install separately and individually so an unsatisfiable
        # -dev on a BSP that shadows the Debian runtime version cannot take
        # the satisfiable deps down with it.
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            libsodium-dev libpcap-dev libevent-dev \
            python3-setuptools 2>&1 | tail -2 || true
        for _devpkg in libgstreamer1.0-dev libgstrtspserver-1.0-dev; do
            DEBIAN_FRONTEND=noninteractive apt-get install -y "${_devpkg}" 2>&1 | tail -1 || true
        done
        FRESH_REPO_DIR="${tmp_repo}" install_wfb_ng_from_vendor

        # Clean up temp repo
        rm -rf "${tmp_repo}"

        # Ensure global symlinks point to current venv
        install_global_symlinks

        # Handle pairing code if provided alongside --upgrade
        if [ -n "$PAIR_CODE" ]; then
            write_pairing "$PAIR_CODE"
        fi

        # Mesh deps on upgrade. Installs batctl + avahi and flips
        # mesh_capable without touching role (stays `direct` until
        # operator sets it). Applied on every ground-station upgrade; a
        # drone-profile node skips this entire block.
        if [ "${ADOS_PROFILE:-}" = "ground_station" ]; then
            install_mesh_deps
        fi

        # RTL8812EU DKMS driver on upgrade for both drone and ground_station
        # profiles. Idempotent: the installer no-ops when the module is
        # already loaded. Earlier releases shipped this for ground-station
        # only, so existing drone rigs need a one-time catch-up here.
        if [ "${ADOS_PROFILE:-}" = "ground_station" ] \
           || [ "${ADOS_PROFILE:-}" = "drone" ]; then
            install_radio_driver_tracked
        fi

        # iw on upgrade. Required by WFB services for TX power control.
        if ! command -v iw >/dev/null 2>&1; then
            DEBIAN_FRONTEND=noninteractive apt-get install -y iw wireless-regdb || \
                warn "iw install failed; WFB services will not be able to set TX power."
        fi

        # Re-apply USB OTG host mode on upgrade. Idempotent: no-op when the
        # controller is already host or when the board has no OTG role node.
        provision_usb_otg_host
        # wfb-ng install moved earlier in the upgrade flow so it can reach
        # the temp-repo's vendor/wfb-ng/ tree before cleanup.

        # Drop first-party plugin trust keys at /etc/ados/plugin-keys/ so
        # the agent can verify signed .adosplug archives. Idempotent
        # against the persisted /opt/ados/source/scripts/plugin-keys
        # path populated by the persist step above.
        provision_plugin_keys
        seed_default_peripherals

        echo ""
        info "Upgrade complete."
        ados_stage_end

        # Same success contract as the fresh-install path. wait_for_api_ready
        # is not called on the upgrade path's print_status (the upgrade path
        # has no print_status call), so the gate's own API probe is the one
        # that matters here. The pairing code prints last, only on success.
        if run_health_gate; then
            ados_install_summary
            print_pairing_code
            exit 0
        fi
        exit 1
    fi

    # ─── Full Install (first time or --force) ───────────────────────────────────

    ADOS_CURRENT_STEP="full-install"
    # Nine operator-facing stages for a fresh install (same count for drone
    # and ground station; the ground-station extras fold into the matching
    # stage). Reset the counter so a resumed run re-numbers from 1. These feed
    # the staging helpers sourced from lib.sh (hence the cross-file disable).
    # shellcheck disable=SC2034
    ADOS_STEP_NUM=0 ADOS_STEP_TOTAL=9
    if $DO_FORCE && is_installed; then
        info "Force reinstall requested. Removing existing venv..."
        rm -rf "${VENV_DIR}"
        # Drop stale checkpoints so a forced reinstall re-runs every step
        # and never trusts a marker from a prior partial run.
        checkpoint_clear
    fi

    ados_stage_begin "Installing system packages"

    # Check or install Python
    PYTHON=$(find_python)
    if [ -z "$PYTHON" ]; then
        info "Python 3.11+ not found. Attempting to install..."
        apt-get update -qq
        # Try python3.12 first (available on Bookworm), then 3.11
        if apt-cache show python3.12 &>/dev/null 2>&1; then
            apt-get install -y -qq python3.12 python3.12-venv python3.12-dev 2>/dev/null
        elif apt-cache show python3.11 &>/dev/null 2>&1; then
            apt-get install -y -qq python3.11 python3.11-venv python3.11-dev 2>/dev/null
        fi
        PYTHON=$(find_python)

        # Distros without a 3.11+ package (Debian 11 bullseye ships 3.9 and
        # has neither python3.11 nor python3.12 in its archives) leave PYTHON
        # empty after the apt attempt. Fall back to a self-contained portable
        # CPython build so the venv can still be created. The provisioner
        # symlinks the interpreter at /usr/local/bin/python3.11, which
        # find_python resolves on the second call.
        if [ -z "$PYTHON" ]; then
            info "No Python 3.11+ available from apt; provisioning a portable interpreter..."
            if provision_portable_python; then
                PYTHON=$(find_python)
            fi
        fi

        if [ -z "$PYTHON" ]; then
            error "Could not install Python 3.11+. Install manually and re-run."
            exit 1
        fi
    fi
    info "Python: ${PYTHON} ($(${PYTHON} --version 2>&1 | awk '{print $2}'))"

    # Install system dependencies (REQUIRED)
    install_system_deps
    checkpoint_mark deps

    # Force USB OTG controller(s) into host mode so a powered hub's
    # downstream radio/camera peripherals enumerate. No-op on boards with
    # no OTG role node (Rockchip / Pi); only acts on controllers reading
    # usb_device.
    provision_usb_otg_host

    # Install mediamtx for video streaming
    install_mediamtx

    # Create directory structure
    info "Creating directories..."
    mkdir -p "${INSTALL_DIR}"
    mkdir -p "${CONFIG_DIR}/certs"
    mkdir -p "${DATA_DIR}/logs/flights"
    mkdir -p "${DATA_DIR}/scripts"
    mkdir -p "${DATA_DIR}/recordings"
    mkdir -p "${INSTALL_DIR}/models/vision"
    mkdir -p "${DATA_DIR}/state"
    ados_stage_end

    ados_stage_begin "Setting up Python environment"
    # Create or refresh the Python venv with system site-packages visible.
    # python3-gi (PyGObject) is an apt-only package — it cannot be pip
    # installed because it links against system libffi/glib/gobject-
    # introspection at build time. The OLED video page's LocalVideoTap
    # does `import gi` to drive its gstreamer pipeline. Without
    # --system-site-packages the agent's venv-isolated Python cannot see
    # the system gi module and the LCD reports "Video pipeline
    # unavailable" forever.
    info "Creating Python virtual environment at ${VENV_DIR}..."
    "$PYTHON" -m venv --system-site-packages "${VENV_DIR}"
    checkpoint_mark venv

    # On a resume (agent present but install incomplete) the venv above
    # already existed, and `python -m venv` leaves an existing tree's pip
    # untouched — so a rotted pip survives into the package install below.
    # Probe + self-heal before any `pip install` runs. No reinstall callback
    # is needed here: the agent-package install steps that immediately follow
    # populate whatever a recreate cleared.
    ensure_venv_pip || warn "venv pip could not be recovered before the package install; the health gate will catch a failed import."
    ados_stage_end

    ados_stage_begin "Installing agent software"
    # Record the active release channel up front so the journal shows which
    # path was taken. stable installs a signed prebuilt wheel + deploy-bundle
    # pinned to a tag; edge (default) clones + builds from source.
    print_channel_banner

    # Provision the install source tree + agent package per channel.
    #
    #   stable — resolve the tag, fetch + verify the signed wheel and the
    #            signed deploy-bundle, unpack the bundle into FRESH_REPO_DIR
    #            (same repo/ layout the edge clone produces), then pip-install
    #            the verified wheel. No on-device source build. Any download
    #            or verification miss hard-fails the install (that is the
    #            point of choosing stable).
    #
    #   edge   — clone the repo (honouring --branch) and pip-install from
    #            source. Unchanged from the historical behaviour.
    #
    # Both channels leave FRESH_REPO_DIR + SYSTEMD_SRC_DIR pointing at a tree
    # whose repo/data/systemd, repo/scripts, repo/vendor are present, so every
    # downstream install helper resolves its source identically.
    FRESH_REPO_DIR=""
    STABLE_TAG=""
    if is_stable_channel; then
        STABLE_TAG="$(resolve_stable_tag)" || {
            error "stable channel: could not resolve a release tag (no v* release found, and no --version pin)."
            record_failure "stable-tag-resolve" required
            run_health_gate || true
            exit 1
        }
        info "Stable channel: pinning to release ${STABLE_TAG}."
        FRESH_REPO_DIR="$(mktemp -d)"
        if ! fetch_and_verify_stable_assets "${STABLE_TAG}" "${FRESH_REPO_DIR}/assets"; then
            error "stable channel: failed to fetch or verify release assets for ${STABLE_TAG}."
            record_failure "stable-assets" required
            run_health_gate || true
            exit 1
        fi
        if ! unpack_deploy_bundle "${STABLE_BUNDLE_PATH}" "${FRESH_REPO_DIR}"; then
            error "stable channel: failed to unpack the verified deploy-bundle."
            record_failure "stable-bundle-unpack" required
            run_health_gate || true
            exit 1
        fi
        SYSTEMD_SRC_DIR="${FRESH_REPO_DIR}/repo/data/systemd"
        export FRESH_REPO_DIR SYSTEMD_SRC_DIR

        # Install the agent package from the verified wheel (REQUIRED). No
        # source build runs on the device.
        info "Installing ados-drone-agent (prebuilt wheel ${STABLE_TAG})..."
        install_agent_from_wheel "${STABLE_WHEEL_PATH}"
        checkpoint_mark agent-package
    else
        # Clone repo for pip install + data files (needed when piped via curl)
        if [ ! -d "$(dirname "$0" 2>/dev/null)/../data/systemd" ] 2>/dev/null; then
            FRESH_REPO_DIR="$(mktemp -d)"
            info "Cloning repository..."
            # Attribute a clone failure to this step (EXIT trap reads it).
            ADOS_CURRENT_STEP="clone-source"
            # honor --branch for feature-branch installs. Bounded retry so a
            # transient network blip does not abort the fresh install.
            if [ -n "$BRANCH_NAME" ]; then
                info "Using branch: ${BRANCH_NAME}"
                git_clone_retry "${FRESH_REPO_DIR}/repo" "${BRANCH_NAME}"
            else
                git_clone_retry "${FRESH_REPO_DIR}/repo"
            fi
            SYSTEMD_SRC_DIR="${FRESH_REPO_DIR}/repo/data/systemd"
        fi
        export FRESH_REPO_DIR SYSTEMD_SRC_DIR

        # Install the agent package (REQUIRED)
        ADOS_CURRENT_STEP="install-agent-package"
        info "Installing ados-drone-agent (this can take a couple of minutes)..."
        ensure_build_toolchain
        if [ -n "${FRESH_REPO_DIR}" ]; then
            ados_with_heartbeat "Installing agent software" \
                "${VENV_DIR}/bin/pip" install "${FRESH_REPO_DIR}/repo" --quiet
        else
            ados_with_heartbeat "Installing agent software" \
                "${VENV_DIR}/bin/pip" install "git+${REPO_URL}" --quiet
        fi
        checkpoint_mark agent-package
    fi
    ados_stage_end

    ados_stage_begin "Configuring radio driver"
    # Resolve agent profile. Ground-station profile pulls extra apt deps,
    # the RTL8812EU DKMS driver, the ground-station python extras, and the
    # mesh dependency bundle (batctl + avahi + wpasupplicant-mesh-sae).
    ADOS_PROFILE="$(resolve_profile)"
    export ADOS_PROFILE
    info "Agent profile: ${ADOS_PROFILE}"

    if [ "${ADOS_PROFILE}" = "ground_station" ]; then
        install_ground_station_deps
        install_radio_driver_tracked

    # Drone profile also needs the RTL8812EU DKMS driver (it's the air side
    # of the WFB-ng radio pair, transmitting). Same idempotent installer
    # the ground-station path uses.
    elif [ "${ADOS_PROFILE}" = "drone" ]; then
        install_radio_driver_tracked
    fi

    if [ "${ADOS_PROFILE}" = "ground_station" ]; then
        info "Installing ground-station Python extras..."
        if is_stable_channel; then
            install_agent_from_wheel "${STABLE_WHEEL_PATH}" ground-station || \
                warn "Ground-station extras install failed; continuing."
        elif [ -n "${FRESH_REPO_DIR}" ]; then
            "${VENV_DIR}/bin/pip" install "${FRESH_REPO_DIR}/repo[ground-station]" --quiet || \
                warn "Ground-station extras install failed; continuing."
        else
            "${VENV_DIR}/bin/pip" install "ados-drone-agent[ground-station] @ git+${REPO_URL}" --quiet || \
                warn "Ground-station extras install failed; continuing."
        fi

        # Mesh dependencies are always installed on the ground-station
        # profile. Small footprint (~8MB) and unused on a `direct` node;
        # the second-USB-WiFi fingerprint in profile_detect sets
        # `mesh_capable: true` when a carrier adapter is present.
        install_mesh_deps
    fi
    ados_stage_end

    ados_stage_begin "Configuring display and identity"
    # SPI LCD on the 40-pin header (e.g. Waveshare 3.5" RPi LCD on Cubie
    # A7Z or Rock 5C). The driver script activates the right device-tree
    # overlay, writes /etc/ados/display.conf, and queues the kernel
    # modules needed at next boot. install_display_driver chooses a
    # profile-aware default: a headless drone defaults to no
    # display so a boot-critical SPI-LCD overlay is never auto-applied
    # against absent hardware; a ground station defaults to auto-detect;
    # an explicit ADOS_DISPLAY always wins. Failure is non-fatal so the
    # agent still boots when the LCD-overlay step fails.
    install_display_driver

    # Generate device identity (idempotent)
    generate_device_id

    # Generate default config (idempotent, skips if exists)
    generate_default_config

    # Write pairing state if code was provided
    if [ -n "$PAIR_CODE" ]; then
        write_pairing "$PAIR_CODE"
    fi
    ados_stage_end

    ados_stage_begin "Installing system services"
    # Install systemd service (REQUIRED — supervisor + profile units)
    install_systemd_service
    checkpoint_mark systemd

    # Persist driver scripts + overlay sources to /opt/ados/source/ so the
    # running agent can re-invoke them later (in particular the wizard's
    # display step). Runs from the freshly-cloned tree before cleanup.
    persist_repo_artifacts
    ados_stage_end

    ados_stage_begin "Building radio link software"
    # wfb-ng userspace from the vendored source. Runs BEFORE the temp-repo
    # cleanup so vendor/wfb-ng/ is still on disk. Idempotent — skips when
    # wfb_tx is already present from a previous install.
    ados_with_heartbeat "Building radio link software" install_wfb_ng_from_vendor

    # Clean up temp repo if we cloned one
    if [ -n "${FRESH_REPO_DIR}" ]; then
        rm -rf "${FRESH_REPO_DIR}"
    fi
    ados_stage_end

    ados_stage_begin "Finalizing setup"
    # Install global symlinks (ados, ados-agent → /usr/local/bin/) (REQUIRED)
    install_global_symlinks
    checkpoint_mark global-symlinks

    # Drop first-party plugin trust keys before the perms pass so they
    # get the same 0600 chmod treatment.
    provision_plugin_keys
    seed_default_peripherals

    # Tighten permissions on any secret-bearing files in /etc/ados. Idempotent;
    # safe to run on every install/upgrade after all file writes have settled.
    harden_secret_perms
    ados_stage_end

    ados_stage_begin "Verifying installation"
    # Print summary (blocks on wait_for_api_ready so AGENT_API_VERSION is set
    # before the health gate re-probes the API).
    print_status

    # Success contract: assert the REQUIRED components are live, write
    # /var/lib/ados/install-result.json, and propagate a non-zero exit when
    # a REQUIRED step failed so the dispatcher's exit code reflects reality.
    # Optional-only failures downgrade to "degraded" and still exit 0. The
    # pairing code is printed last, only on success, so it is the final thing
    # the operator sees.
    if run_health_gate; then
        ados_stage_end
        ados_install_summary
        print_pairing_code
        exit 0
    fi
    exit 1
}
