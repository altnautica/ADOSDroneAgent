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

    # ─── Fast Path: Already installed, no flags ──────────────────────────────────

    if is_installed && ! $DO_FORCE && ! $DO_UPGRADE; then
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

    # ─── Upgrade Path (skip apt, skip venv creation) ────────────────────────────

    if is_installed && $DO_UPGRADE && ! $DO_FORCE; then
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
        if [ "${ADOS_PROFILE}" = "ground_station" ] || [ "${ADOS_PROFILE}" = "ground-station" ]; then
            mask_conflicting_standalone_services
        fi

        # Ensure system deps are present. The upgrade path skips the full
        # install_system_deps to keep upgrades fast, so we only top up the
        # packages that earlier installs may have missed. Includes the
        # wfb-ng runtime Python deps (twisted et al.) so wfb-server can
        # start the bind protocol on rigs first installed before v0.16.4.
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

        # Clone repo to temp dir for pip install + systemd files + install script
        tmp_repo="$(mktemp -d)"
        info "Fetching latest source..."
        # honor --branch for feature-branch installs
        if [ -n "$BRANCH_NAME" ]; then
            info "Using branch: ${BRANCH_NAME}"
            git clone --depth 1 --recurse-submodules --shallow-submodules --quiet --branch "${BRANCH_NAME}" "${REPO_URL}" "${tmp_repo}/repo"
        else
            git clone --depth 1 --recurse-submodules --shallow-submodules --quiet "${REPO_URL}" "${tmp_repo}/repo"
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

        # Upgrade pip package from cloned source (ensures version match)
        info "Upgrading pip package..."
        "${VENV_DIR}/bin/pip" install --upgrade "${tmp_repo}/repo" --quiet

        new_ver=$(get_installed_version)
        if [ "$local_ver" = "$new_ver" ]; then
            info "Already on latest version (${new_ver})."
        else
            info "Upgraded: ${local_ver} -> ${new_ver}"
        fi

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
        if [ "${ADOS_PROFILE}" = "ground_station" ] || [ "${ADOS_PROFILE}" = "ground-station" ]; then
            enable_ground_station_units
        fi

        # Config migration: a brief 0.26.7/0.26.8 release rewrote the REST
        # API host to "::" expecting kernel-default dual-stack. uvicorn on
        # some Pi kernels treated [::] as IPv6-only and IPv4 connections
        # were refused. Now the agent binds explicit dual-stack sockets
        # at startup, so the config host should be the IPv4 wildcard
        # again. Flip "::" back to "0.0.0.0" idempotently.
        cfg_file="${CONFIG_DIR}/config.yaml"
        if [ -f "$cfg_file" ] && grep -q '^[[:space:]]*host:[[:space:]]*"::"' "$cfg_file"; then
            info "Reverting REST API bind from '::' to '0.0.0.0' (config.yaml; agent now dual-binds at startup)"
            sed -i 's|^\([[:space:]]*\)host:[[:space:]]*"::"|\1host: "0.0.0.0"|' "$cfg_file"
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
        # the function short-circuits on boards whose HAL has no
        # displays.supported entry, so calling it on a drone rig with no
        # LCD is a fast no-op.
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

        # Persist driver scripts + overlay sources to /opt/ados/source/ so the
        # wizard's display step (and any future CLI re-runs) can find them
        # without a fresh git clone.
        FRESH_REPO_DIR="${tmp_repo}" persist_repo_artifacts

        # wfb-ng userspace from the vendored source — must run BEFORE the
        # temp-repo cleanup so vendor/wfb-ng/ is still on disk. Build deps
        # are best-effort; the function bails clean if anything is missing.
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            libsodium-dev libpcap-dev libevent-dev \
            libgstreamer1.0-dev libgstrtspserver-1.0-dev \
            python3-setuptools 2>&1 | tail -2 || true
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
        if [ "${ADOS_PROFILE:-}" = "ground_station" ] || [ "${ADOS_PROFILE:-}" = "ground-station" ]; then
            install_mesh_deps
        fi

        # RTL8812EU DKMS driver on upgrade for both drone and ground_station
        # profiles. Idempotent: the installer no-ops when the module is
        # already loaded. Earlier releases shipped this for ground-station
        # only, so existing drone rigs need a one-time catch-up here.
        if [ "${ADOS_PROFILE:-}" = "ground_station" ] \
           || [ "${ADOS_PROFILE:-}" = "ground-station" ] \
           || [ "${ADOS_PROFILE:-}" = "drone" ]; then
            install_ground_station_driver
        fi

        # iw on upgrade. Required by WFB services for TX power control.
        if ! command -v iw >/dev/null 2>&1; then
            DEBIAN_FRONTEND=noninteractive apt-get install -y iw wireless-regdb || \
                warn "iw install failed; WFB services will not be able to set TX power."
        fi
        # wfb-ng install moved earlier in the upgrade flow so it can reach
        # the temp-repo's vendor/wfb-ng/ tree before cleanup.

        # Drop first-party plugin trust keys at /etc/ados/plugin-keys/ so
        # the agent can verify signed .adosplug archives. Idempotent
        # against the persisted /opt/ados/source/scripts/plugin-keys
        # path populated by the persist step above.
        provision_plugin_keys

        echo ""
        info "Upgrade complete."
        print_pairing_code
        exit 0
    fi

    # ─── Full Install (first time or --force) ───────────────────────────────────

    if $DO_FORCE && is_installed; then
        info "Force reinstall requested. Removing existing venv..."
        rm -rf "${VENV_DIR}"
    fi

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
        if [ -z "$PYTHON" ]; then
            error "Could not install Python 3.11+. Install manually and re-run."
            exit 1
        fi
    fi
    info "Python: ${PYTHON} ($(${PYTHON} --version 2>&1 | awk '{print $2}'))"

    # Install system dependencies
    install_system_deps

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

    # Clone repo for pip install + data files (needed when piped via curl)
    FRESH_REPO_DIR=""
    if [ ! -d "$(dirname "$0" 2>/dev/null)/../data/systemd" ] 2>/dev/null; then
        FRESH_REPO_DIR="$(mktemp -d)"
        info "Cloning repository..."
        # honor --branch for feature-branch installs
        if [ -n "$BRANCH_NAME" ]; then
            info "Using branch: ${BRANCH_NAME}"
            git clone --depth 1 --recurse-submodules --shallow-submodules --quiet --branch "${BRANCH_NAME}" "${REPO_URL}" "${FRESH_REPO_DIR}/repo"
        else
            git clone --depth 1 --recurse-submodules --shallow-submodules --quiet "${REPO_URL}" "${FRESH_REPO_DIR}/repo"
        fi
        SYSTEMD_SRC_DIR="${FRESH_REPO_DIR}/repo/data/systemd"
    fi
    export FRESH_REPO_DIR SYSTEMD_SRC_DIR

    # Install the agent package
    info "Installing ados-drone-agent..."
    "${VENV_DIR}/bin/pip" install --upgrade pip --quiet
    if [ -n "${FRESH_REPO_DIR}" ]; then
        "${VENV_DIR}/bin/pip" install "${FRESH_REPO_DIR}/repo" --quiet
    else
        "${VENV_DIR}/bin/pip" install "git+${REPO_URL}" --quiet
    fi

    # Resolve agent profile. Ground-station profile pulls extra apt deps,
    # the RTL8812EU DKMS driver, the ground-station python extras, and the
    # mesh dependency bundle (batctl + avahi + wpasupplicant-mesh-sae).
    ADOS_PROFILE="$(resolve_profile)"
    export ADOS_PROFILE
    info "Agent profile: ${ADOS_PROFILE}"

    if [ "${ADOS_PROFILE}" = "ground_station" ] || [ "${ADOS_PROFILE}" = "ground-station" ]; then
        install_ground_station_deps
        install_ground_station_driver

    # Drone profile also needs the RTL8812EU DKMS driver (it's the air side
    # of the WFB-ng radio pair, transmitting). Same idempotent installer
    # the ground-station path uses.
    elif [ "${ADOS_PROFILE}" = "drone" ]; then
        install_ground_station_driver
    fi

    if [ "${ADOS_PROFILE}" = "ground_station" ] || [ "${ADOS_PROFILE}" = "ground-station" ]; then
        info "Installing ground-station Python extras..."
        if [ -n "${FRESH_REPO_DIR}" ]; then
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

    # SPI LCD on the 40-pin header (e.g. Waveshare 3.5" RPi LCD on Cubie
    # A7Z or Rock 5C). The driver script auto-detects the board and the
    # supported display, compiles or activates the right device-tree
    # overlay, writes /etc/ados/display.conf, and queues the kernel
    # modules needed at next boot. Runs unconditionally so a drone-profile
    # rig with a status LCD is provisioned the same way; the function
    # itself short-circuits on boards whose HAL profile carries no
    # displays.supported entry. Failure is non-fatal so the agent still
    # boots when the LCD-overlay step fails.
    install_display_driver

    # Generate device identity (idempotent)
    generate_device_id

    # Generate default config (idempotent, skips if exists)
    generate_default_config

    # Write pairing state if code was provided
    if [ -n "$PAIR_CODE" ]; then
        write_pairing "$PAIR_CODE"
    fi

    # Install systemd service
    install_systemd_service

    # Persist driver scripts + overlay sources to /opt/ados/source/ so the
    # running agent can re-invoke them later (in particular the wizard's
    # display step). Runs from the freshly-cloned tree before cleanup.
    persist_repo_artifacts

    # wfb-ng userspace from the vendored source. Runs BEFORE the temp-repo
    # cleanup so vendor/wfb-ng/ is still on disk. Idempotent — skips when
    # wfb_tx is already present from a previous install.
    install_wfb_ng_from_vendor

    # Clean up temp repo if we cloned one
    if [ -n "${FRESH_REPO_DIR}" ]; then
        rm -rf "${FRESH_REPO_DIR}"
    fi

    # Install global symlinks (ados, ados-agent → /usr/local/bin/)
    install_global_symlinks

    # Drop first-party plugin trust keys before the perms pass so they
    # get the same 0600 chmod treatment.
    provision_plugin_keys

    # Tighten permissions on any secret-bearing files in /etc/ados. Idempotent;
    # safe to run on every install/upgrade after all file writes have settled.
    harden_secret_perms

    # Print summary
    print_status
    print_pairing_code
}
