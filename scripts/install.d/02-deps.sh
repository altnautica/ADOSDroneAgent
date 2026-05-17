# shellcheck shell=bash
# =============================================================================
# 02-deps.sh — apt package installers for the base agent and ground-station.
#
# install_system_deps lays down the cross-profile base; install_ground_station_deps
# pulls hostapd / dnsmasq / chromium / cage and reshapes Pi-family boot
# config for USB gadget mode. Both functions are idempotent and tolerate
# missing packages on minimal Buildroot rootfs builds.
# =============================================================================

# ─── System Dependencies (Linux) ────────────────────────────────────────────

install_system_deps() {
    info "Installing system dependencies..."

    # Hold packages known to break mid-install on Radxa BSP images before
    # touching apt. u-boot-rk2410 postinst can trigger a mid-install reboot;
    # aic8800-usb-dkms has a broken 5.0+git build that fails DKMS compile
    # and leaves apt in a half-configured state. These holds are idempotent
    # and safe on boards where the packages are not present.
    for pkg in u-boot-rk2410 aic8800-usb-dkms radxa-system-config-aic8800-usb-dkms; do
        if dpkg -l "$pkg" 2>/dev/null | grep -q "^ii"; then
            apt-mark hold "$pkg" >/dev/null 2>&1 || true
        fi
    done

    apt-get update

    # Core: Python venv, pip, dev headers for native extensions
    # libcap-dev: Linux capabilities (for low-level device access)
    # libsystemd-dev: systemd notify protocol
    # libyaml-dev: fast YAML parsing (PyYAML C extension)
    # Use v4l-utils (the Debian Bookworm package); v4l2-utils is wrong and
    # breaks the install. Do not hide apt errors with 2>/dev/null; let
    # failures surface to the install log.
    # python3-gi + gir typelib give us the in-process gstreamer binding
    # the LCD video page uses to attach an appsink to MediaMTX. The
    # gstreamer1.0-plugins-bad / -ugly + gstreamer1.0-libav bundles
    # provide rtph264depay, avdec_h264, and the v4l2 H.264 decoder used
    # on Allwinner / Amlogic boards that ship a working V4L2 stateful
    # decoder. Hardware Rockchip MPP support is opt-in below.
    apt-get install -y \
        python3-venv \
        python3-pip \
        python3-dev \
        python3-setuptools \
        python3-twisted \
        python3-serial \
        python3-jinja2 \
        python3-msgpack \
        python3-pyroute2 \
        python3-gi \
        gir1.2-gstreamer-1.0 \
        socat \
        libcap-dev \
        libsystemd-dev \
        libyaml-dev \
        libsodium-dev \
        libpcap-dev \
        libevent-dev \
        libgstreamer1.0-dev \
        libgstrtspserver-1.0-dev \
        build-essential \
        git \
        curl \
        avahi-daemon \
        ffmpeg \
        v4l-utils \
        gstreamer1.0-tools \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-ugly \
        gstreamer1.0-libav \
        gstreamer1.0-rtsp \
        iw \
        wireless-regdb

    # Try the Rockchip MPP plugin opportunistically. The package only
    # exists on Radxa BSP repos that have shipped a build for the SoC
    # at hand; an Orange Pi or generic-arm64 rootfs simply doesn't have
    # it, and we fall through to the upstream V4L2 decoder. The "|| true"
    # makes apt failure non-fatal so a missing package does not break
    # the rest of the install.
    if [ -r /proc/device-tree/model ]; then
        _install_model="$(tr -d '\000' < /proc/device-tree/model 2>/dev/null || true)"
        if printf '%s' "${_install_model}" | grep -qiE 'rockchip|rk3588|rk3568|rk3566|rk3582|rk3576'; then
            info "Rockchip board detected, attempting hardware MPP plugin install"
            apt-get install -y gstreamer1.0-rockchip-mpp || \
                info "gstreamer1.0-rockchip-mpp not available for this board (software decode will be used)"
        fi
    fi

    info "System dependencies installed."
}

# Extra apt deps needed for the ground-station profile. Idempotent.
install_ground_station_deps() {
    info "Installing ground-station profile dependencies..."

    # The Chromium package is named `chromium` on Debian 13 trixie and
    # later, `chromium-browser` on Debian 12 bookworm and the older
    # Radxa BSPs. Pick by reading /etc/os-release so neither side logs
    # a noisy fallback warning on the platforms it does run on.
    local chromium_pkg="chromium-browser"
    local os_version_codename=""
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        os_version_codename="${VERSION_CODENAME:-}"
    fi
    case "${os_version_codename}" in
        trixie|forky|sid)
            chromium_pkg="chromium"
            ;;
    esac

    if ! apt-get install -y \
        hostapd \
        dnsmasq \
        bluetooth \
        bluez \
        "${chromium_pkg}" \
        cage; then
        # Belt-and-suspenders: if the chosen chromium package fails for
        # any reason, retry with the other name. Any remaining failure
        # is silenced (||true) because the rest of the GS profile runs
        # without chromium just fine — the kiosk service only matters
        # when an HDMI panel is attached.
        local fallback_pkg="chromium"
        [ "${chromium_pkg}" = "chromium" ] && fallback_pkg="chromium-browser"
        warn "Primary ground-station deps install failed; retrying chromium as ${fallback_pkg}."
        apt-get install -y hostapd dnsmasq bluetooth bluez cage || true
        apt-get install -y "${fallback_pkg}" || true
    fi

    mask_conflicting_standalone_services

    # Ensure dwc2 overlay + module load for USB gadget mode (Pi family).
    local cfg="/boot/firmware/config.txt"
    if [ ! -f "${cfg}" ] && [ -f "/boot/config.txt" ]; then
        cfg="/boot/config.txt"
    fi
    if [ -f "${cfg}" ]; then
        if ! grep -qE '^\s*dtoverlay=dwc2' "${cfg}"; then
            info "Appending dtoverlay=dwc2 to ${cfg}"
            printf '\n# ADOS ground-station profile: USB gadget mode\ndtoverlay=dwc2\n' >> "${cfg}"
        fi
    else
        warn "Boot config not found; skipping dtoverlay=dwc2 append."
    fi

    local cmdline="/boot/firmware/cmdline.txt"
    if [ ! -f "${cmdline}" ] && [ -f "/boot/cmdline.txt" ]; then
        cmdline="/boot/cmdline.txt"
    fi
    if [ -f "${cmdline}" ]; then
        if ! grep -q 'modules-load=dwc2' "${cmdline}"; then
            info "Appending modules-load=dwc2 to ${cmdline}"
            # cmdline.txt is single-line; append before the trailing newline
            sed -i 's/$/ modules-load=dwc2/' "${cmdline}"
        fi
    else
        warn "Boot cmdline not found; skipping modules-load=dwc2 append."
    fi

    # Optional modem stack. Skipped by default so ground stations without
    # cellular hardware do not pull ~80 MB of ModemManager + libqmi +
    # libmbim just to stare at them. Set `ADOS_ENABLE_MODEM=1` in the
    # install environment to opt in.
    if [ "${ADOS_ENABLE_MODEM:-0}" = "1" ]; then
        info "ADOS_ENABLE_MODEM=1 set; installing ModemManager + QMI/MBIM utilities..."
        apt-get install -y modemmanager libqmi-utils libmbim-utils || \
            warn "Modem stack install failed; ados-modem.service will run in AT fallback mode only."
    else
        info "Skipping modem stack (set ADOS_ENABLE_MODEM=1 to install modemmanager + libqmi-utils + libmbim-utils)."
    fi

    # Optional share_uplink firewall persistence. Skipped by default
    # because share_uplink is opt-in and pulling iptables-persistent on
    # every ground station that never plans to NAT for AP clients is
    # wasteful. Set `ADOS_ENABLE_SHARE_UPLINK=1` to install
    # iptables-persistent on Debian/Raspbian. On non-Debian or buildroot
    # images we skip the apt install and let the runtime helper fall
    # back to nftables (when present) for persistence.
    if [ "${ADOS_ENABLE_SHARE_UPLINK:-0}" = "1" ]; then
        if command -v apt-get >/dev/null 2>&1; then
            info "ADOS_ENABLE_SHARE_UPLINK=1 set; installing iptables-persistent..."
            DEBIAN_FRONTEND=noninteractive \
                debconf-set-selections <<<'iptables-persistent iptables-persistent/autosave_v4 boolean true' || true
            DEBIAN_FRONTEND=noninteractive \
                debconf-set-selections <<<'iptables-persistent iptables-persistent/autosave_v6 boolean true' || true
            DEBIAN_FRONTEND=noninteractive apt-get install -y iptables iptables-persistent || \
                warn "iptables-persistent install failed; share_uplink will use nftables fallback if available."
        else
            info "Non-Debian image; skipping iptables-persistent. share_uplink helper will use nftables fallback when 'nft' is present."
        fi
    else
        info "Skipping share_uplink firewall persistence (set ADOS_ENABLE_SHARE_UPLINK=1 to install iptables-persistent on Debian)."
    fi

    # NetworkManager is mandatory for the WiFi client manager (nmcli
    # backend). Enable + start if installed but inactive. Radxa BSPs ship
    # with it but sometimes leave it masked.
    if systemctl list-unit-files NetworkManager.service >/dev/null 2>&1; then
        if ! systemctl is-active --quiet NetworkManager.service; then
            info "Enabling and starting NetworkManager..."
            systemctl unmask NetworkManager.service 2>/dev/null || true
            systemctl enable NetworkManager.service 2>/dev/null || true
            systemctl start NetworkManager.service 2>/dev/null || \
                warn "NetworkManager start failed; WiFi client + uplink router will be degraded."
        else
            info "NetworkManager already active."
        fi
    else
        warn "NetworkManager not installed. WiFi client manager will fail. Install with: apt-get install network-manager"
    fi

    info "Ground-station deps installed."
}
