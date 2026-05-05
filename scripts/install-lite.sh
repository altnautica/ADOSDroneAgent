#!/usr/bin/env bash
# =============================================================================
# ADOS Drone Agent — Lightweight Rust Backend Installer
# Supports: Raspberry Pi OS, Buildroot rootfs (Luckfox class), and any glibc
#           or musl Linux with init = systemd, busybox, or runit.
# Usage:    sudo ./install-lite.sh [PAIR_CODE]
#           sudo ./install-lite.sh --uninstall
# Idempotent: re-runs are safe and update in place.
#
# Verifies the prebuilt binary against an Ed25519 signature (minisign) and a
# SHA256 checksum before installing. The public key is vendored below.
# =============================================================================

set -euo pipefail

GITHUB_OWNER="altnautica"
GITHUB_REPO="ADOSDroneAgent"
RELEASE_CHANNEL="${ADOS_RELEASE_CHANNEL:-stable}"  # stable | main
INSTALL_BIN="/usr/local/bin/ados-agent-lite"
CONFIG_DIR="/etc/ados"
CONFIG_PATH="${CONFIG_DIR}/agent.yaml"
SYSTEMD_UNIT="/etc/systemd/system/ados-agent-lite.service"
SYSV_INIT_SCRIPT="/etc/init.d/S99ados-agent-lite"

# Vendored Ed25519 public key for release-artifact verification. Replace the
# placeholder below with the real public key produced from the team's
# minisign keypair generation.
MINISIGN_PUBLIC_KEY="${ADOS_LITE_MINISIGN_PUBLIC_KEY:-RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8}"

log() { printf '[install-lite] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        die "this installer must run as root (use sudo)"
    fi
}

detect_target() {
    local arch libc
    arch="$(uname -m)"
    libc="glibc"
    # Detect musl by checking ldd output. Musl's ldd prints "musl libc" on
    # the first line; glibc prints "ldd (...)".
    if ldd --version 2>&1 | head -n1 | grep -qi musl; then
        libc="musl"
    fi

    case "${arch}-${libc}" in
        armv7l-*)
            echo "armv7-unknown-linux-musleabihf"  # only musl variant published today
            ;;
        aarch64-glibc)
            echo "aarch64-unknown-linux-gnu"
            ;;
        aarch64-musl)
            echo "aarch64-unknown-linux-musl"
            ;;
        x86_64-musl)
            echo "x86_64-unknown-linux-musl"
            ;;
        x86_64-glibc)
            # No glibc x86_64 release artifact today; the musl variant is
            # statically linked and runs on glibc hosts too.
            echo "x86_64-unknown-linux-musl"
            ;;
        *)
            die "unsupported target: arch=${arch}, libc=${libc}"
            ;;
    esac
}

detect_init_system() {
    if [ -d /run/systemd/system ] || command -v systemctl >/dev/null 2>&1 && systemctl is-system-running >/dev/null 2>&1; then
        echo "systemd"
    elif [ -x /sbin/openrc-run ] || [ -d /etc/runlevels ]; then
        echo "openrc"
    elif command -v rc-service >/dev/null 2>&1; then
        echo "openrc"
    elif [ -x /etc/init.d/rcS ] || [ -d /etc/init.d ]; then
        echo "busybox"
    else
        echo "unknown"
    fi
}

resolve_release_url() {
    local target="$1"
    local version
    if [ "${RELEASE_CHANNEL}" = "main" ]; then
        # Rolling main: assets live under tag 'lite-agent-main'.
        version="lite-agent-main"
    else
        # Latest stable: query the GitHub API for the most recent
        # 'lite-v*' tag.
        version="$(curl -sSL "https://api.github.com/repos/${GITHUB_OWNER}/${GITHUB_REPO}/releases" \
            | grep -E '"tag_name"\s*:\s*"lite-v' \
            | head -n1 \
            | sed -E 's/.*"tag_name"\s*:\s*"(lite-v[^"]+)".*/\1/')"
        [ -n "${version}" ] || die "no stable lite-v* release found"
    fi

    echo "https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases/download/${version}"
}

download() {
    local url="$1" dest="$2"
    log "fetching ${url}"
    curl -fSL --retry 3 --retry-delay 2 -o "${dest}" "${url}"
}

verify_artifact() {
    local artifact="$1" base
    base="$(basename "${artifact}")"

    # SHA256 check.
    if [ -f "${artifact}.sha256" ]; then
        log "verifying SHA256 of ${base}"
        ( cd "$(dirname "${artifact}")" && sha256sum -c "${base}.sha256" >/dev/null )
    else
        die "missing ${base}.sha256 — refusing to install unsigned/unchecked artifact"
    fi

    # Ed25519 signature. Mandatory — a network-positioned attacker who
    # ensures minisign is not pre-installed must not be able to bypass
    # signature checks. Operators who genuinely need to skip can set
    # ADOS_LITE_ALLOW_UNSIGNED=1 explicitly.
    if [ "${ADOS_LITE_ALLOW_UNSIGNED:-0}" = "1" ]; then
        log "warn: ADOS_LITE_ALLOW_UNSIGNED=1 set; skipping minisign signature verification"
        return 0
    fi
    if ! command -v minisign >/dev/null 2>&1; then
        log "minisign is required but not installed"
        log "install via: apt-get install -y minisign  (Debian/Ubuntu)"
        log "         or: apk add minisign            (Alpine/Buildroot)"
        log "         or: brew install minisign       (macOS)"
        log "to bypass at your own risk, set ADOS_LITE_ALLOW_UNSIGNED=1"
        die "signature verification cannot proceed without minisign"
    fi
    if [ ! -f "${artifact}.minisig" ]; then
        die "missing ${base}.minisig — refusing to install unsigned artifact"
    fi
    log "verifying minisign signature of ${base}"
    minisign -V -P "${MINISIGN_PUBLIC_KEY}" -m "${artifact}" -x "${artifact}.minisig" >/dev/null
}

extract_binary() {
    local artifact="$1" workdir
    workdir="$(mktemp -d)"
    tar -xzf "${artifact}" -C "${workdir}"
    [ -f "${workdir}/ados-agent-lite" ] || die "extracted artifact missing ados-agent-lite binary"
    install -m 0755 "${workdir}/ados-agent-lite" "${INSTALL_BIN}"
    rm -rf "${workdir}"
    log "installed binary at ${INSTALL_BIN}"
}

generate_device_id() {
    # Stable per-device identifier. Try /etc/machine-id first
    # (systemd / Buildroot both populate it on first boot); fall back
    # to uuidgen, then to a hostname + epoch hash.
    if [ -r /etc/machine-id ]; then
        printf 'ados-%s' "$(cat /etc/machine-id | tr -d '\n')"
        return
    fi
    if command -v uuidgen >/dev/null 2>&1; then
        printf 'ados-%s' "$(uuidgen | tr -d '\n')"
        return
    fi
    printf 'ados-%s-%s' "$(hostname 2>/dev/null || echo unknown)" "$(date +%s)"
}

write_default_config() {
    if [ -f "${CONFIG_PATH}" ]; then
        log "config already present at ${CONFIG_PATH}; leaving untouched"
        return 0
    fi
    install -d -m 0755 "${CONFIG_DIR}"
    local device_id
    device_id="$(generate_device_id)"
    cat > "${CONFIG_PATH}" <<EOF
# ADOS lightweight agent configuration.
# See proto/setup/setup-api.yaml + proto/cloud/openapi.yaml for field semantics.

agent:
  device_id: "${device_id}"
  name: "ADOS Lite"

mavlink:
  port: "/dev/ttyS0"
  baud: 115200

cloud:
  # mqtt_broker / convex_url / api_key are populated by the pairing
  # flow. Until those values are set the agent runs offline (MAVLink
  # router only) and the HTTPS layer emits a pairing beacon to register
  # this device with the cloud relay.
  mqtt_broker: ""
  mqtt_port: 8883
  mqtt_use_tls: true
  convex_url: ""
  api_key: ""

api:
  bind: "127.0.0.1:8080"
EOF
    chmod 0644 "${CONFIG_PATH}"
    log "wrote default config at ${CONFIG_PATH} (device_id: ${device_id})"
}

install_systemd_unit() {
    cat > "${SYSTEMD_UNIT}" <<EOF
[Unit]
Description=ADOS Lightweight Drone Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_BIN} --config ${CONFIG_PATH} run
Restart=on-failure
RestartSec=2
User=root
LimitNOFILE=4096

[Install]
WantedBy=multi-user.target
EOF
    chmod 0644 "${SYSTEMD_UNIT}"
    systemctl daemon-reload
    systemctl enable ados-agent-lite.service >/dev/null
    systemctl restart ados-agent-lite.service
    log "started systemd service: ados-agent-lite.service"
}

install_busybox_init() {
    cat > "${SYSV_INIT_SCRIPT}" <<EOF
#!/bin/sh
# ADOS lightweight agent init script (busybox sysv-rc).

DAEMON=${INSTALL_BIN}
CONFIG=${CONFIG_PATH}
PIDFILE=/var/run/ados-agent-lite.pid

case "\$1" in
    start)
        echo "Starting ados-agent-lite..."
        start-stop-daemon -S -b -m -p "\$PIDFILE" \\
            --exec "\$DAEMON" -- --config "\$CONFIG" run
        ;;
    stop)
        echo "Stopping ados-agent-lite..."
        start-stop-daemon -K -p "\$PIDFILE" --quiet
        rm -f "\$PIDFILE"
        ;;
    restart)
        \$0 stop || true
        sleep 1
        \$0 start
        ;;
    status)
        if [ -f "\$PIDFILE" ] && kill -0 "\$(cat "\$PIDFILE")" 2>/dev/null; then
            echo "ados-agent-lite running"
            exit 0
        else
            echo "ados-agent-lite not running"
            exit 1
        fi
        ;;
    *)
        echo "Usage: \$0 {start|stop|restart|status}"
        exit 1
        ;;
esac
EOF
    chmod 0755 "${SYSV_INIT_SCRIPT}"
    "${SYSV_INIT_SCRIPT}" restart
    log "started busybox init: ${SYSV_INIT_SCRIPT}"
}

uninstall() {
    if [ -f "${SYSTEMD_UNIT}" ]; then
        systemctl stop ados-agent-lite.service 2>/dev/null || true
        systemctl disable ados-agent-lite.service 2>/dev/null || true
        rm -f "${SYSTEMD_UNIT}"
        systemctl daemon-reload
    fi
    if [ -x "${SYSV_INIT_SCRIPT}" ]; then
        "${SYSV_INIT_SCRIPT}" stop 2>/dev/null || true
        rm -f "${SYSV_INIT_SCRIPT}"
    fi
    rm -f "${INSTALL_BIN}"
    log "uninstalled lightweight agent"
    log "config at ${CONFIG_PATH} preserved; remove manually if desired"
}

main() {
    require_root

    if [ "${1:-}" = "--uninstall" ]; then
        uninstall
        return 0
    fi

    local pair_code="${1:-}"
    local target init_system release_url artifact tmpdir

    target="$(detect_target)"
    init_system="$(detect_init_system)"
    log "target architecture:  ${target}"
    log "detected init system: ${init_system}"

    release_url="$(resolve_release_url "${target}")"
    artifact="ados-agent-lite-*-${target}.tar.gz"

    tmpdir="$(mktemp -d)"
    pushd "${tmpdir}" >/dev/null

    # Find the artifact matching this target. Two query paths: the public
    # GitHub API (returns asset list as JSON), and as a fallback the HTML
    # asset-listing endpoint.
    local listing artifact_url sums_url sig_url release_tag api_url
    release_tag="$(basename "${release_url}")"
    api_url="https://api.github.com/repos/${GITHUB_OWNER}/${GITHUB_REPO}/releases/tags/${release_tag}"
    listing="$(curl -fsSL "${api_url}" 2>/dev/null \
        | grep -oE '"name"\s*:\s*"ados-agent-lite-[^"]+-'"${target}"'\.tar\.gz"' \
        | head -n1 \
        | sed -E 's/.*"name"\s*:\s*"([^"]+)".*/\1/')"
    if [ -z "${listing}" ]; then
        # Fallback: scrape the HTML expanded_assets endpoint.
        listing="$(curl -fsSL "${release_url%/download/*}/expanded_assets/${release_tag}" 2>/dev/null \
            | grep -oE "ados-agent-lite-[^\"]+-${target}\.tar\.gz" \
            | head -n1)"
    fi
    if [ -z "${listing}" ]; then
        die "could not resolve artifact name for ${target} from ${release_tag}; check the release page at https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases"
    fi
    artifact_url="${release_url}/${listing}"
    sums_url="${artifact_url}.sha256"
    sig_url="${artifact_url}.minisig"

    download "${artifact_url}" "${listing}"
    download "${sums_url}"     "${listing}.sha256" || die "missing SHA256 alongside artifact"
    download "${sig_url}"      "${listing}.minisig" || log "warn: missing minisign signature; proceeding with SHA256 only"

    verify_artifact "${listing}"
    extract_binary "${listing}"

    popd >/dev/null
    rm -rf "${tmpdir}"

    write_default_config

    if [ -n "${pair_code}" ]; then
        log "pairing code provided; persist via: ados-agent-lite pair ${pair_code} (subcommand pending)"
    fi

    case "${init_system}" in
        systemd)  install_systemd_unit ;;
        busybox)  install_busybox_init ;;
        openrc)
            log "openrc init detected; manual unit installation required (no template shipped at v0.1)"
            ;;
        *)
            log "unknown init system; binary installed at ${INSTALL_BIN} but no service unit configured"
            ;;
    esac

    log "done. Config: ${CONFIG_PATH}"
}

main "$@"
