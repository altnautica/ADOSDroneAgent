# shellcheck shell=bash
# =============================================================================
# 05-mesh.sh — distributed RX mesh dependencies (batctl, avahi, wpasup).
#
# Runs on the ground-station profile only. Packages are small (~8MB) and
# unused on a `direct` node; the node's role stays `direct` until the
# operator explicitly promotes it.
# =============================================================================

# Distributed RX mesh dependencies. Always runs on ground-station profile
# (packages are small, ~8MB, and unused on a `direct` node). Installs
# batctl + avahi-daemon + wpasupplicant with mesh backend, writes the
# mesh_capable flag into /etc/ados/profile.conf, and leaves the node's
# role at `direct` so existing deployments are not auto-promoted into mesh.
install_mesh_deps() {
    info "Installing mesh dependencies..."

    if command -v apt-get >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            batctl \
            avahi-daemon \
            wpasupplicant \
            iw || {
            warn "Mesh deps install failed; ados-batman.service will not start."
        }

        # wpad-mesh-wolfssl carries the SAE (802.11s authentication)
        # backend on Raspbian/Debian. Best-effort: not every release
        # ships it. IBSS carrier fallback works without it.
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            wpasupplicant-mesh-sae 2>/dev/null || \
            DEBIAN_FRONTEND=noninteractive apt-get install -y \
            wpad-mesh-wolfssl 2>/dev/null || \
            info "802.11s SAE backend not available via apt; IBSS fallback will apply."
    else
        warn "apt-get not found; skipping mesh deps. Install batctl + avahi-daemon manually."
    fi

    # Ensure /etc/ados/ exists (may run before ground-station deps on a
    # fresh install) then flip the mesh_capable flag in profile.conf.
    mkdir -p /etc/ados
    local pc="/etc/ados/profile.conf"
    if [ -f "${pc}" ]; then
        if grep -q '^mesh_capable:' "${pc}"; then
            sed -i 's/^mesh_capable:.*/mesh_capable: true/' "${pc}"
        else
            echo "mesh_capable: true" >> "${pc}"
        fi
    else
        cat > "${pc}" <<EOF
profile: auto
mesh_capable: true
EOF
    fi

    # Ensure the mesh identity directory exists (0o755; the PSK file
    # inside stays 0o600 and is written by mesh_manager on first boot
    # for receivers or by pairing_manager for relays).
    mkdir -p /etc/ados/mesh
    chmod 755 /etc/ados/mesh

    # Enable the pairing daemon so UDP 5801 can survive REST restarts.
    # REST continues to use the in-process PairingManager by default;
    # operators flip ADOS_PAIRING_VIA_DAEMON=1 in /etc/ados/env to
    # opt into the split topology.
    if [ -f "/etc/systemd/system/ados-mesh-pairing.service" ]; then
        systemctl enable ados-mesh-pairing.service 2>/dev/null || true
    fi

    info "Mesh capability enabled. Role stays 'direct' until set via OLED -> Mesh or 'ados gs role set <role>'."
}
