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
    # The bind protocol needs wfb-server (the Python orchestrator from
    # setup.py) AND the wifibroadcast@.service template, not just the
    # `make all_bin` C binaries. Earlier installs that built only
    # `wfb_tx` / `wfb_rx` / `wfb_keygen` are insufficient — they pass the
    # legacy `command -v wfb_tx` gate but lack everything the local
    # bind window needs. Treat that case as a partial install and
    # force a rebuild so setup.py runs end-to-end.
    if command -v wfb_tx >/dev/null 2>&1 \
        && command -v wfb-server >/dev/null 2>&1 \
        && { [ -f /usr/lib/systemd/system/wifibroadcast@.service ] \
             || [ -f /etc/systemd/system/wifibroadcast@.service ]; }; then
        info "wfb-ng already installed: $(command -v wfb_tx)"
        provision_wfb_bind_artifacts ""
        return 0
    fi
    if command -v wfb_tx >/dev/null 2>&1; then
        info "wfb-ng partial install detected (missing wfb-server or unit template); rebuilding"
    fi

    local vendor_dir=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -d "${FRESH_REPO_DIR}/repo/vendor/wfb-ng" ]; then
        vendor_dir="${FRESH_REPO_DIR}/repo/vendor/wfb-ng"
    elif [ -d "$(dirname "$0" 2>/dev/null)/../vendor/wfb-ng" ] 2>/dev/null; then
        vendor_dir="$(cd "$(dirname "$0")/../vendor/wfb-ng" && pwd)"
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
}
