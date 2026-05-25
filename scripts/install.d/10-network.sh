# shellcheck shell=bash
# =============================================================================
# 10-network.sh — out-of-band network binary installs.
#
# Today this only carries the mediamtx binary downloader. Future helpers
# that fetch network tools (cloudflared, tailscaled, custom relays)
# belong here.
# =============================================================================

# ─── MediaMTX Installation ─────────────────────────────────────────────────

install_mediamtx() {
    info "Checking mediamtx..."
    if command -v mediamtx &>/dev/null; then
        info "mediamtx already installed: $(which mediamtx)"
        return 0
    fi

    local arch
    arch="$(detect_arch)"
    local mtx_arch
    case "$arch" in
        aarch64) mtx_arch="arm64" ;;
        armhf)   mtx_arch="armv7" ;;
        x86_64)  mtx_arch="amd64" ;;
        *)
            warn "Unsupported architecture for mediamtx: $arch"
            return 1
            ;;
    esac

    local url="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/mediamtx_v${MEDIAMTX_VERSION}_linux_${mtx_arch}.tar.gz"
    local tmp_dir
    tmp_dir="$(mktemp -d)"

    info "Downloading mediamtx v${MEDIAMTX_VERSION} for ${mtx_arch}..."
    # Route through ados_fetch for uniform retry/backoff. ados_fetch is
    # sourced by the orchestration module from scripts/lib/net.sh; fall
    # back to a direct curl when it is unavailable (e.g. this module run
    # in isolation) so the downloader never hard-depends on source order.
    local fetched=false
    if declare -F ados_fetch >/dev/null 2>&1; then
        if ados_fetch "$url" "$tmp_dir/mediamtx.tar.gz" 120; then
            fetched=true
        fi
    elif curl -fSL --retry 3 --retry-delay 2 "$url" -o "$tmp_dir/mediamtx.tar.gz"; then
        fetched=true
    fi

    if [ "${fetched}" = "true" ]; then
        tar -xzf "$tmp_dir/mediamtx.tar.gz" -C "$tmp_dir"
        install -m 755 "$tmp_dir/mediamtx" /usr/local/bin/mediamtx
        info "mediamtx installed to /usr/local/bin/mediamtx"
    else
        warn "Failed to download mediamtx — video streaming will not work"
    fi

    rm -rf "$tmp_dir"
}
