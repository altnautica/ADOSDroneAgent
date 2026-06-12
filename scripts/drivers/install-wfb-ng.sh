#!/bin/bash
# =============================================================================
# install-wfb-ng.sh — build + install the wfb-ng userspace from the vendored
# source tree, then provision the local-radio bind artifacts.
#
# Delegated to from the installer's `wfb_ng` step (mirrors how the RTL8812EU
# driver build is delegated to install-rtl8812eu.sh): the heavy build +
# provisioning is leaf OS shell work the installer ORCHESTRATES, not rewrites.
#
# Builds wfb_tx, wfb_rx, wfb_keygen, wfb-server, wfb_bind_{server,client}.sh and
# the wifibroadcast@.service template from vendor/wfb-ng/. The bind protocol
# needs wfb-server (the Python orchestrator from setup.py) AND the unit template,
# not just the C binaries, so a C-only partial install forces a rebuild.
#
# Provisions:
#   /etc/bind.key   shared default bind-channel key (the per-pair link keys are
#                   still minted fresh by wfb_keygen at bind time)
#   /etc/bind.yaml  wfb-server bind profiles, pinned to home channel 149
#   /etc/systemd/system/wifibroadcast@.service  bind-profile unit template
#   /etc/ados/wfb-ng.version  vendored commit SHA (drift between paired rigs
#                   silently breaks the bind tunnel)
#
# Idempotent: re-running skips the rebuild when wfb-ng is already at the vendored
# commit, and re-asserts the bind artifacts (so an upgrade lands them too).
# =============================================================================

log()  { printf '[install-wfb-ng] %s\n' "$*"; }
warn() { printf '[install-wfb-ng] WARN: %s\n' "$*" >&2; }

# Resolve the vendored wfb-ng source relative to this script
# (scripts/drivers/install-wfb-ng.sh -> ../../vendor/wfb-ng). On a real SBC the
# source tree is persisted at /opt/ados/source, so this lands at
# /opt/ados/source/vendor/wfb-ng.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VENDOR_DIR="$(cd "${SCRIPT_DIR}/../.." 2>/dev/null && pwd)/vendor/wfb-ng"

# Provision the artifacts the supervisor's local-radio bind state machine drives.
provision_wfb_bind_artifacts() {
    if [ ! -f /etc/bind.key ]; then
        log "Writing default /etc/bind.key (upstream wfb-ng shared bind key)"
        echo "RvrSKeUVjoU/xXaYTWC+7AtlVdhvuQlhw5UvdlkM84L80RfATVid7J7y/dVnm48LCsmB1hRhPtgkxNe0kmB9Dg==" \
            | base64 -d > /etc/bind.key
        chmod 0644 /etc/bind.key
    fi

    if [ ! -f /etc/bind.yaml ] && command -v wfb-server >/dev/null 2>&1; then
        log "Generating /etc/bind.yaml via wfb-server --gen-bind-yaml"
        if ! wfb-server --gen-bind-yaml --profiles drone drone_bind gs gs_bind \
             > /etc/bind.yaml.tmp 2>/dev/null; then
            warn "wfb-server --gen-bind-yaml failed; bind protocol unavailable."
            rm -f /etc/bind.yaml.tmp
        else
            mv /etc/bind.yaml.tmp /etc/bind.yaml
            chmod 0644 /etc/bind.yaml
        fi
    fi

    # Pin the bind channel to the link's home channel (149). wfb-server defaults
    # the bind profiles to 165 (top of U-NII-3), which many regulatory domains
    # cap at zero TX power or disable; an RTL8812EU then emits no 802.11 frames
    # there and the drone<->ground bind silently never crosses the air. 149 is
    # widely permitted and is the link's rendezvous-home channel. Idempotent.
    if [ -f /etc/bind.yaml ] && grep -q 'wifi_channel: 165' /etc/bind.yaml 2>/dev/null; then
        log "Pinning bind channel to 149 (165 is TX-restricted in many regions)"
        sed -i 's/wifi_channel: 165/wifi_channel: 149/g' /etc/bind.yaml
    fi

    # Install the wfb-ng template unit so wifibroadcast@drone_bind /
    # wifibroadcast@gs_bind are addressable via systemctl. setup.py ships it in
    # the deb layout under /usr/lib/systemd/system, but bare-bones rootfs builds
    # ignore that dir; mirror to /etc/systemd/system to make it unambiguous.
    local _unit_src=""
    if [ -f /usr/lib/systemd/system/wifibroadcast@.service ]; then
        _unit_src="/usr/lib/systemd/system/wifibroadcast@.service"
    elif [ -f "${VENDOR_DIR}/scripts/systemd/wifibroadcast@.service" ]; then
        _unit_src="${VENDOR_DIR}/scripts/systemd/wifibroadcast@.service"
    fi
    if [ -n "${_unit_src}" ] && [ ! -f /etc/systemd/system/wifibroadcast@.service ]; then
        log "Installing wifibroadcast@.service template from ${_unit_src}"
        install -m 0644 "${_unit_src}" /etc/systemd/system/wifibroadcast@.service
    fi
    if [ -f /etc/systemd/system/wifibroadcast@.service ] || [ -f /usr/lib/systemd/system/wifibroadcast@.service ]; then
        systemctl daemon-reload >/dev/null 2>&1 || true
    fi

    # Patch wfb_bind_server.sh so wfb-server's stdin is /dev/null (not the held
    # socat TCP socket). Without it the version subprocess never sees EOF, the
    # bash $(...) substitution blocks forever, and the key-transfer window burns
    # with zero bytes moved. Idempotent (second sed is a no-op).
    if [ -f /usr/bin/wfb_bind_server.sh ] \
        && grep -q 'wfb-server --version | head' /usr/bin/wfb_bind_server.sh; then
        log "Patching wfb_bind_server.sh show_version to close wfb-server stdin"
        sed -i 's@wfb-server --version | head@wfb-server --version </dev/null | head@' \
            /usr/bin/wfb_bind_server.sh
    fi

    # Stop wfb-server from running its own NetworkManager dance on the injection
    # adapter. wfb-server's init (set_nm_unmanaged=True in master.cfg) runs a
    # `nmcli device show <iface>` pre-check and ABORTS on a non-zero rc. On a
    # stock NetworkManager box, NM does not reliably enumerate a monitor-mode RTL
    # injection iface (it never created a device object for it), so the pre-check
    # returns RC 10 "device not found" and the bind aborts every cycle (observed
    # on a drone where the ground node, whose NM happened to enumerate the same
    # adapter, bound fine). The agent already owns the adapter's NM + monitor-mode
    # lifecycle (it releases it from NM and sets verified monitor before starting
    # the bind unit), so wfb-ng must not duplicate that. A site config that pins
    # set_nm_unmanaged = False makes wfb-server skip the pre-check and proceed
    # straight to its own down/monitor/up. Point the bind profiles at it (the
    # vendored env files default WIFIBROADCAST_CFG to /dev/null, which would
    # otherwise mask this file). Idempotent.
    #
    # wifi_region pins the regulatory domain wfb-server asserts when a bind
    # profile starts. The vendored master.cfg default is 'BO', and init_wlans
    # runs `iw reg set <region>` unconditionally — so every bind start would
    # set the GLOBAL cfg80211 domain to BO, which kills the onboard management
    # WiFi's data path (associated + leased but gateway ARP INCOMPLETE) until
    # the agent's regulatory reconciler wins the domain back. US permits both
    # the ch149/165 operating/rendezvous channels and matches the reconciler's
    # pinned domain, so the bind no longer fights it. Note: a successful bind
    # pushes this file from the ground station to the drone (the wire protocol
    # transfers wifibroadcast.cfg), so both rigs converge on it by design.
    printf '%s\n' '[common]' 'set_nm_unmanaged = False' "wifi_region = 'US'" > /etc/wifibroadcast.cfg
    chmod 0644 /etc/wifibroadcast.cfg
    for _be in /etc/default/wifibroadcast.drone_bind /etc/default/wifibroadcast.gs_bind; do
        if [ -f "${_be}" ] && grep -q 'WIFIBROADCAST_CFG=/dev/null' "${_be}" 2>/dev/null; then
            log "Pointing $(basename "${_be}") at /etc/wifibroadcast.cfg (skip the nmcli pre-check)"
            sed -i 's|WIFIBROADCAST_CFG=/dev/null|WIFIBROADCAST_CFG=/etc/wifibroadcast.cfg|' "${_be}"
        fi
    done
}

# Build + install the wfb-ng userspace from the vendored source.
install_wfb_ng_from_vendor() {
    local _vendor_commit="" _installed_commit=""
    if [ -e "${VENDOR_DIR}/.git" ]; then
        _vendor_commit="$(git -C "${VENDOR_DIR}" rev-parse HEAD 2>/dev/null || true)"
    fi
    if [ -r /etc/ados/wfb-ng.version ]; then
        _installed_commit="$(head -c 80 /etc/ados/wfb-ng.version 2>/dev/null | tr -d '[:space:]')"
    fi

    # Skip the rebuild only when wfb-server + the unit template are present AND
    # the recorded commit matches the vendored source (or the vendor commit is
    # unknown). A C-binaries-only install passes a bare `command -v wfb_tx` gate
    # but lacks everything the local bind window needs, so treat it as partial.
    if command -v wfb_tx >/dev/null 2>&1 \
        && command -v wfb-server >/dev/null 2>&1 \
        && { [ -f /usr/lib/systemd/system/wifibroadcast@.service ] \
             || [ -f /etc/systemd/system/wifibroadcast@.service ]; } \
        && { [ -z "${_vendor_commit}" ] || [ "${_installed_commit}" = "${_vendor_commit}" ]; }; then
        log "wfb-ng already installed: $(command -v wfb_tx)"
        provision_wfb_bind_artifacts
        return 0
    fi

    if [ -z "${VENDOR_DIR}" ] || [ ! -f "${VENDOR_DIR}/Makefile" ]; then
        warn "wfb-ng vendored source not found at ${VENDOR_DIR}; ensure submodules were cloned."
        return 0
    fi

    log "Building wfb-ng from vendored source at ${VENDOR_DIR}..."
    # wfb_rtsp needs librga (Rockchip RGA), absent on some BSPs. We don't use it
    # at runtime (the bind drives wfb-server, not the RTSP demo), so on a
    # librga-less build, build everything else and stub wfb_rtsp so setup.py's
    # data_files copy still succeeds. The bind protocol works either way.
    if ! ( cd "${VENDOR_DIR}" && make all_bin wfb_rtsp gs.key >/tmp/wfb-ng-build.log 2>&1 ); then
        log "wfb-ng full build failed (librga / gstreamer-rtsp deps); retrying without wfb_rtsp"
        if ! ( cd "${VENDOR_DIR}" && make all_bin gs.key >/tmp/wfb-ng-build.log 2>&1 ); then
            warn "wfb-ng minimal build also failed; see /tmp/wfb-ng-build.log."
            return 0
        fi
        touch "${VENDOR_DIR}/wfb_rtsp"
    fi

    log "Installing wfb-ng binaries to /usr/bin..."
    # setup.py asserts VERSION + COMMIT are in the environment; harvest them the
    # same way the upstream Makefile does (`make version`), with safe fallbacks.
    local _env _ver _commit _epoch
    _env="$( cd "${VENDOR_DIR}" && make version 2>/dev/null )"
    _ver="$(    printf '%s\n' "${_env}" | awk -F= '/^VERSION=/{print $2; exit}' )"
    _commit="$( printf '%s\n' "${_env}" | awk -F= '/^COMMIT=/{print $2; exit}' )"
    _epoch="$(  printf '%s\n' "${_env}" | awk -F= '/^SOURCE_DATE_EPOCH=/{print $2; exit}' )"
    : "${_ver:=0.0.0}"; : "${_commit:=release}"; : "${_epoch:=$(date +%s)}"
    log "wfb-ng version=${_ver} commit=${_commit:0:8}"
    if ! ( cd "${VENDOR_DIR}" && \
           VERSION="${_ver}" COMMIT="${_commit}" SOURCE_DATE_EPOCH="${_epoch}" \
           /usr/bin/python3 setup.py install --root=/ --install-layout=deb >/tmp/wfb-ng-install.log 2>&1 ); then
        # Fall back to a plain install when --install-layout=deb is unavailable
        # (debian helpers absent on some BSP images).
        if ! ( cd "${VENDOR_DIR}" && \
               VERSION="${_ver}" COMMIT="${_commit}" SOURCE_DATE_EPOCH="${_epoch}" \
               /usr/bin/python3 setup.py install >/tmp/wfb-ng-install.log 2>&1 ); then
            warn "wfb-ng install failed; see /tmp/wfb-ng-install.log."
            return 0
        fi
    fi

    if command -v wfb_tx >/dev/null 2>&1; then
        log "wfb-ng installed: $(command -v wfb_tx)"
        # Persist the vendored commit so a later upgrade can detect drift and
        # rebuild. Both rigs in a pair MUST run the same wfb-ng commit.
        if [ -n "${_vendor_commit}" ]; then
            mkdir -p /etc/ados
            printf '%s\n' "${_vendor_commit}" > /etc/ados/wfb-ng.version
        fi
    else
        warn "wfb-ng install ran but wfb_tx not on PATH; check setup.py data_files paths."
    fi

    # We only reach here on a rebuild (the already-at-commit fast path returned
    # early). A wfb-ng version bump can change the bind profiles, so drop a stale
    # /etc/bind.yaml first and let provision regenerate it from the new
    # wfb-server. Fresh installs have no file yet, so this is a no-op there.
    rm -f /etc/bind.yaml
    provision_wfb_bind_artifacts
}

install_wfb_ng_from_vendor
