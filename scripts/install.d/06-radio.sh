# shellcheck shell=bash
# =============================================================================
# 06-radio.sh — wfb-ng userspace build/install + bind artifact provisioning.
#
# Builds the vendored wfb-ng source tree (vendor/wfb-ng/) for wfb_tx,
# wfb_rx, wfb_keygen, wfb-server, and the wifibroadcast@.service unit
# template. provision_wfb_bind_artifacts drops the default bind key,
# bind.yaml, and the systemd template so the local-radio bind
# orchestrator can run wfb-server out of the box.
# =============================================================================

# Build and install wfb-ng userspace (wfb_tx, wfb_rx, wfb_keygen,
# wfb_tx_cmd, wfb_tun) from the vendored source tree at
# vendor/wfb-ng/. Idempotent: skips if wfb_tx is already on PATH.
# Installs to /usr/bin/ via the upstream setup.py data_files mapping,
# so the binaries are reachable from the systemd unit's default PATH
# without any extra Environment= directive.
install_wfb_ng_from_vendor() {
    # Resolve vendor_dir up front so we can compare the vendored source
    # commit against whatever is currently installed. Version drift
    # between two rigs in a pair causes silent bind-tunnel failures
    # (FEC sessions converge but the L3 TCP handshake never lands).
    local vendor_dir=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -d "${FRESH_REPO_DIR}/repo/vendor/wfb-ng" ]; then
        vendor_dir="${FRESH_REPO_DIR}/repo/vendor/wfb-ng"
    elif [ -d "$(dirname "$0" 2>/dev/null)/../vendor/wfb-ng" ] 2>/dev/null; then
        vendor_dir="$(cd "$(dirname "$0")/../vendor/wfb-ng" && pwd)"
    fi

    # .git is a directory in a regular clone and a gitlink file in a
    # submodule clone; -e covers both so the commit probe lands in both
    # layouts.
    local _vendor_commit=""
    if [ -n "${vendor_dir}" ] && [ -e "${vendor_dir}/.git" ]; then
        _vendor_commit="$(git -C "${vendor_dir}" rev-parse HEAD 2>/dev/null || true)"
    fi
    local _installed_commit=""
    if [ -r /etc/ados/wfb-ng.version ]; then
        _installed_commit="$(head -c 80 /etc/ados/wfb-ng.version 2>/dev/null | tr -d '[:space:]')"
    fi

    # The bind protocol needs wfb-server (the Python orchestrator from
    # setup.py) AND the wifibroadcast@.service template, not just the
    # `make all_bin` C binaries. Earlier installs that built only
    # `wfb_tx` / `wfb_rx` / `wfb_keygen` are insufficient — they pass the
    # legacy `command -v wfb_tx` gate but lack everything the local
    # bind window needs. Treat that case as a partial install and
    # force a rebuild so setup.py runs end-to-end.
    #
    # Additionally, skip the rebuild ONLY when the recorded installed
    # commit matches the vendored source commit. When the vendored
    # commit cannot be determined (no .git, persisted source tree without
    # history) keep the original behavior and trust the existing install.
    if command -v wfb_tx >/dev/null 2>&1 \
        && command -v wfb-server >/dev/null 2>&1 \
        && { [ -f /usr/lib/systemd/system/wifibroadcast@.service ] \
             || [ -f /etc/systemd/system/wifibroadcast@.service ]; } \
        && { [ -z "${_vendor_commit}" ] || [ "${_installed_commit}" = "${_vendor_commit}" ]; }; then
        if [ -n "${_vendor_commit}" ]; then
            info "wfb-ng already at vendored commit ${_vendor_commit:0:8}: $(command -v wfb_tx)"
        else
            info "wfb-ng already installed: $(command -v wfb_tx)"
        fi
        provision_wfb_bind_artifacts "${vendor_dir}"
        return 0
    fi
    if command -v wfb_tx >/dev/null 2>&1; then
        if [ -n "${_vendor_commit}" ] && [ "${_installed_commit}" != "${_vendor_commit}" ]; then
            info "wfb-ng commit drift detected (installed=${_installed_commit:0:8} vendor=${_vendor_commit:0:8}); rebuilding"
        else
            info "wfb-ng partial install detected (missing wfb-server or unit template); rebuilding"
        fi
    fi

    if [ -z "${vendor_dir}" ] || [ ! -f "${vendor_dir}/Makefile" ]; then
        warn "wfb-ng vendored source not found at vendor/wfb-ng/; ensure submodules were cloned."
        return 0
    fi

    info "Building wfb-ng from vendored source at ${vendor_dir}..."
    # setup.py's data_files expects all_bin + wfb_rtsp + gs.key. The
    # wfb_rtsp target needs librga (Rockchip RGA) which isn't on every
    # BSP — Radxa Bookworm on the Rock 5C, for example. We don't use
    # wfb_rtsp at runtime (the bind protocol drives wfb-server, not
    # the RTSP demo), so on librga-less BSPs we build everything else
    # and stub wfb_rtsp with a zero-byte file so setup.py can still
    # copy it. The bind protocol works either way.
    if ! ( cd "${vendor_dir}" && make all_bin wfb_rtsp gs.key >/tmp/wfb-ng-build.log 2>&1 ); then
        info "wfb-ng full build failed (librga / gstreamer-rtsp deps); retrying without wfb_rtsp"
        if ! ( cd "${vendor_dir}" && make all_bin gs.key >/tmp/wfb-ng-build.log 2>&1 ); then
            warn "wfb-ng minimal build also failed; see /tmp/wfb-ng-build.log."
            return 0
        fi
        touch "${vendor_dir}/wfb_rtsp"
    fi

    info "Installing wfb-ng binaries to /usr/bin..."
    # setup.py asserts that VERSION and COMMIT are present in the
    # environment; the upstream Makefile derives them via git describe
    # and exports both. Use `make version` to harvest the same KEY=VALUE
    # pairs and source them into our shell. Fall back to safe defaults
    # if anything in the chain fails.
    set +e
    local _wfb_make_env _wfb_version _wfb_commit _wfb_epoch
    _wfb_make_env="$( cd "${vendor_dir}" && make version 2>/dev/null )"
    _wfb_version="$( printf '%s\n' "${_wfb_make_env}" | awk -F= '/^VERSION=/{print $2; exit}' )"
    _wfb_commit="$(  printf '%s\n' "${_wfb_make_env}" | awk -F= '/^COMMIT=/{print $2; exit}'  )"
    _wfb_epoch="$(   printf '%s\n' "${_wfb_make_env}" | awk -F= '/^SOURCE_DATE_EPOCH=/{print $2; exit}' )"
    set -e
    : "${_wfb_version:=0.0.0}"
    : "${_wfb_commit:=release}"
    : "${_wfb_epoch:=$(date +%s)}"
    info "wfb-ng version=${_wfb_version} commit=${_wfb_commit:0:8}"
    if ! ( cd "${vendor_dir}" && \
           VERSION="${_wfb_version}" COMMIT="${_wfb_commit}" SOURCE_DATE_EPOCH="${_wfb_epoch}" \
           /usr/bin/python3 setup.py install --root=/ --install-layout=deb >/tmp/wfb-ng-install.log 2>&1 ); then
        # Fall back to standard install if --install-layout=deb is not
        # available (debian helpers absent on some Radxa BSP images).
        if ! ( cd "${vendor_dir}" && \
               VERSION="${_wfb_version}" COMMIT="${_wfb_commit}" SOURCE_DATE_EPOCH="${_wfb_epoch}" \
               /usr/bin/python3 setup.py install >/tmp/wfb-ng-install.log 2>&1 ); then
            warn "wfb-ng install failed; see /tmp/wfb-ng-install.log."
            return 0
        fi
    fi

    if command -v wfb_tx >/dev/null 2>&1; then
        info "wfb-ng installed: $(command -v wfb_tx)"
        # Persist the vendored commit so the next install/upgrade can
        # detect drift and rebuild. Both rigs in a pair must run the
        # same wfb-ng commit; mismatched builds silently break the bind
        # tunnel (FEC frames decode but L3 TCP never completes).
        if [ -n "${_vendor_commit}" ]; then
            mkdir -p /etc/ados
            printf '%s\n' "${_vendor_commit}" > /etc/ados/wfb-ng.version
        fi
    else
        warn "wfb-ng install ran but wfb_tx not on PATH; check setup.py data_files paths."
    fi

    provision_wfb_bind_artifacts "${vendor_dir}"
}

# Provision artifacts for the local-radio bind protocol used by the
# Python bind orchestrator (services/wfb/bind_orchestrator.py):
#   /etc/bind.key      hardcoded default shared key (matches upstream)
#   /etc/bind.yaml     wfb-server profiles for drone_bind / gs_bind
#   wifibroadcast@.service  systemd template for the bind profile
# Idempotent. Runs both on a fresh wfb-ng build and on every upgrade
# (the install_wfb_ng_from_vendor early-return path also calls this so
# rigs that had wfb-ng before v0.16 land the bind artifacts on upgrade).
provision_wfb_bind_artifacts() {
    local vendor_dir="$1"

    if [ ! -f /etc/bind.key ]; then
        info "Writing default /etc/bind.key (upstream wfb-ng shared bind key)"
        echo "RvrSKeUVjoU/xXaYTWC+7AtlVdhvuQlhw5UvdlkM84L80RfATVid7J7y/dVnm48LCsmB1hRhPtgkxNe0kmB9Dg==" \
            | base64 -d > /etc/bind.key
        chmod 0644 /etc/bind.key
    fi

    if [ ! -f /etc/bind.yaml ] && command -v wfb-server >/dev/null 2>&1; then
        info "Generating /etc/bind.yaml via wfb-server --gen-bind-yaml"
        if ! wfb-server --gen-bind-yaml --profiles drone drone_bind gs gs_bind \
             > /etc/bind.yaml.tmp 2>/dev/null; then
            warn "wfb-server --gen-bind-yaml failed; bind protocol unavailable."
            rm -f /etc/bind.yaml.tmp
        else
            mv /etc/bind.yaml.tmp /etc/bind.yaml
            chmod 0644 /etc/bind.yaml
        fi
    fi

    # Pin the bind channel to the link's home channel (149). wfb-server's
    # --gen-bind-yaml defaults the bind profiles to channel 165 (top of the
    # U-NII-3 band), which many regulatory domains cap at zero TX power or
    # disable outright. On those domains an RTL8812EU emits no 802.11 frames
    # on 165, so the drone<->ground bind never crosses the air and pairing
    # silently fails (the key-transfer tunnel times out with no bytes moved).
    # Channel 149 is the link's home channel and is widely permitted, so bind
    # and data stay on one known-good channel. Runs on every install/upgrade
    # because the generation step above only fires when the file is absent;
    # idempotent once the channel is already 149.
    if [ -f /etc/bind.yaml ] && grep -q 'wifi_channel: 165' /etc/bind.yaml 2>/dev/null; then
        info "Pinning bind channel to 149 (165 is TX-restricted in many regions)"
        sed -i 's/wifi_channel: 165/wifi_channel: 149/g' /etc/bind.yaml
    fi

    # Install the wfb-ng template unit so `wifibroadcast@drone_bind` and
    # `wifibroadcast@gs_bind` are addressable via systemctl. setup.py
    # ships it in the deb layout at /usr/lib/systemd/system/, but Radxa
    # BSP and bare-bones rootfs builds sometimes ignore that directory;
    # mirror to /etc/systemd/system to make the unit unambiguously
    # visible.
    local _wfb_unit_src=""
    if [ -f /usr/lib/systemd/system/wifibroadcast@.service ]; then
        _wfb_unit_src="/usr/lib/systemd/system/wifibroadcast@.service"
    elif [ -n "${vendor_dir}" ] && [ -f "${vendor_dir}/scripts/systemd/wifibroadcast@.service" ]; then
        _wfb_unit_src="${vendor_dir}/scripts/systemd/wifibroadcast@.service"
    fi
    if [ -n "${_wfb_unit_src}" ] && [ ! -f /etc/systemd/system/wifibroadcast@.service ]; then
        info "Installing wifibroadcast@.service template from ${_wfb_unit_src}"
        install -m 0644 "${_wfb_unit_src}" /etc/systemd/system/wifibroadcast@.service
    fi
    if [ -f /etc/systemd/system/wifibroadcast@.service ] || [ -f /usr/lib/systemd/system/wifibroadcast@.service ]; then
        systemctl daemon-reload >/dev/null 2>&1 || true
    fi

    # Patch the bind server's show_version handler so wfb-server's stdin
    # is /dev/null instead of inheriting the long-lived socat TCP socket.
    # Without the redirect, wfb-server prints its version line but the
    # Twisted reactor keeps watching stdin (which never closes while
    # socat holds the connection) and the subprocess never exits. The
    # bash $(...) substitution then blocks forever, the client's read
    # times out, and the entire 300 s key-transfer window burns with
    # zero bytes transferred. Idempotent — second run of sed is a no-op
    # because the redirect is already in place.
    if [ -f /usr/bin/wfb_bind_server.sh ] \
        && grep -q 'wfb-server --version | head' /usr/bin/wfb_bind_server.sh; then
        info "Patching wfb_bind_server.sh show_version to close wfb-server stdin"
        # Use @ as sed delimiter because the pattern and replacement both
        # contain | (the shell pipe) — would collide with the default
        # / delimiter and the older | delimiter spelling. The pattern
        # matches the bare "--version |" pipe; the replacement inserts
        # "</dev/null" between --version and the pipe so wfb-server
        # sees EOF on stdin immediately.
        sed -i 's@wfb-server --version | head@wfb-server --version </dev/null | head@' \
            /usr/bin/wfb_bind_server.sh
    fi
}
