# shellcheck shell=bash
# =============================================================================
# 08-plugin.sh — plugin slice + tmpfiles.d provisioning.
#
# install_plugin_slice wraps scripts/setup-plugin-slice.sh so the cgroup
# slice contents live in one place. install_plugin_tmpfiles materialises
# /run/ados/plugins so per-plugin Unix sockets have a home before the
# supervisor or any plugin service starts.
# =============================================================================

# Install the shared cgroup slice that hosts third-party plugin
# services. Idempotent and fail-soft: the rest of the agent works
# without it, but plugins cannot launch until the slice is in place.
# Wraps scripts/setup-plugin-slice.sh so the slice contents and
# directory layout stay in one place.
install_plugin_slice() {
    info "Installing plugin slice..."

    local slice_script=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -x "${FRESH_REPO_DIR}/repo/scripts/setup-plugin-slice.sh" ]; then
        slice_script="${FRESH_REPO_DIR}/repo/scripts/setup-plugin-slice.sh"
    elif [ -n "${SYSTEMD_SRC_DIR:-}" ] && [ -x "$(dirname "${SYSTEMD_SRC_DIR}")/../scripts/setup-plugin-slice.sh" ]; then
        slice_script="$(cd "$(dirname "${SYSTEMD_SRC_DIR}")/../scripts" && pwd)/setup-plugin-slice.sh"
    elif [ -x "$(dirname "$0" 2>/dev/null)/setup-plugin-slice.sh" ] 2>/dev/null; then
        slice_script="$(cd "$(dirname "$0")" && pwd)/setup-plugin-slice.sh"
    fi

    if [ -z "${slice_script}" ] || [ ! -x "${slice_script}" ]; then
        warn "Plugin slice setup script not found; plugin services will not start until ados-plugins.slice is provisioned."
        return 0
    fi

    if "${slice_script}"; then
        info "Plugin slice ready."
    else
        warn "Plugin slice setup failed; plugins will not start until resolved (run scripts/setup-plugin-slice.sh manually)."
    fi
}

# Drop the plugin tmpfiles.d snippet into /etc/tmpfiles.d and
# materialize the runtime directory. Idempotent: safe to re-run.
# Sources the snippet from the repo first; falls back to a literal
# write so older trees and minimal install paths still get the
# directory provisioned.
install_plugin_tmpfiles() {
    info "Installing plugin tmpfiles..."

    local snippet_src=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/etc/tmpfiles.d/ados-plugins.conf" ]; then
        snippet_src="${FRESH_REPO_DIR}/repo/etc/tmpfiles.d/ados-plugins.conf"
    elif [ -n "${SYSTEMD_SRC_DIR:-}" ] && [ -f "$(dirname "${SYSTEMD_SRC_DIR}")/../etc/tmpfiles.d/ados-plugins.conf" ]; then
        snippet_src="$(cd "$(dirname "${SYSTEMD_SRC_DIR}")/../etc/tmpfiles.d" && pwd)/ados-plugins.conf"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../etc/tmpfiles.d/ados-plugins.conf" ] 2>/dev/null; then
        snippet_src="$(cd "$(dirname "$0")/../etc/tmpfiles.d" && pwd)/ados-plugins.conf"
    fi

    if [ -n "${snippet_src}" ] && [ -f "${snippet_src}" ]; then
        cp "${snippet_src}" /etc/tmpfiles.d/ados-plugins.conf
    else
        cat > /etc/tmpfiles.d/ados-plugins.conf <<PLUGTMPEOF
# ADOS plugin runtime sockets and runtime state
d /run/ados/plugins 0750 ados ados -
r! /run/ados/plugins/*.sock
PLUGTMPEOF
    fi
    chmod 0644 /etc/tmpfiles.d/ados-plugins.conf

    if command -v systemd-tmpfiles >/dev/null 2>&1; then
        systemd-tmpfiles --create /etc/tmpfiles.d/ados-plugins.conf >/dev/null 2>&1 || true
    else
        # No systemd-tmpfiles on this host; create the directory
        # directly. The ados user may not exist yet when this runs in
        # very early provisioning, so skip chown silently in that case.
        mkdir -p /run/ados/plugins
        chmod 0750 /run/ados/plugins
        chown ados:ados /run/ados/plugins 2>/dev/null || true
    fi
}
