#!/usr/bin/env bash
# =============================================================================
# ADOS Drone Agent — Lightweight Rust Backend Installer
# Supports: Raspberry Pi OS, Buildroot rootfs (Luckfox class), and any glibc
#           or musl Linux with init = systemd, busybox, or runit.
# Usage:    sudo ./install-lite.sh                       (install unpaired)
#           sudo ./install-lite.sh PAIRCODE              (install + pair)
#           sudo ./install-lite.sh --pair PAIRCODE       (same, named flag)
#           sudo ./install-lite.sh --upgrade             (re-pull latest binary, preserve config)
#           sudo ./install-lite.sh --skip-verify         (development only; bypass signature check)
#           sudo ./install-lite.sh --uninstall
#                ./install-lite.sh --show-key            (print embedded public key + fingerprint)
# Idempotent: re-runs are safe and update in place.
#
# When invoked without a pairing code the agent installs in unpaired mode
# and broadcasts a pairing beacon. Operators pair later via:
#   - the setup webapp at http://<board-ip>:8080/setup
#   - the CLI:  sudo ados-agent-lite pair PAIRCODE
#   - Mission Control "Add drone" with the beacon code shown by the agent
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

# Vendored Ed25519 public key for release-artifact verification. The CI
# release pipeline replaces this string with the real public key on tag.
# We deliberately do NOT accept an env override for the public key — that
# would let an attacker who controls the operator's environment substitute
# their own signing key and pass verification on a malicious binary.
# Rotation is a code change + git push, not a runtime knob.
MINISIGN_PUBLIC_KEY="RWQprWT1xlflXCT6CLpuSHyw8UuXlji88f+8JrW9V9E9ynE2iJX7LlfW"
PLACEHOLDER_KEY="RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8"

# Short fingerprint of the active public key. The CI release pipeline
# replaces this string with the real fingerprint at tag time. Operators
# can read it via `./install-lite.sh --show-key` and compare to the
# fingerprint printed at the top of the release notes.
MINISIGN_PUBLIC_KEY_FINGERPRINT="5CE557C6F564AD29"

log() { printf '[install-lite] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# Fetch helper: prefers curl when available, falls back to wget. Buildroot
# images (Luckfox SDK, etc.) ship with wget but not curl. We treat both as
# equivalent for HTTP/HTTPS GETs. Output goes to a file when $2 is given,
# otherwise to stdout.
_fetch() {
    local url="$1" outfile="${2:-}" timeout_secs="${3:-30}"
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${outfile}" ]; then
            curl -fsSL --max-time "${timeout_secs}" --retry 3 --retry-delay 2 \
                -o "${outfile}" "${url}"
        else
            curl -fsSL --max-time "${timeout_secs}" --retry 3 --retry-delay 2 \
                "${url}"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ -n "${outfile}" ]; then
            wget -q -T "${timeout_secs}" --tries=3 -O "${outfile}" "${url}"
        else
            wget -q -T "${timeout_secs}" --tries=3 -O - "${url}"
        fi
    else
        die "neither curl nor wget is installed; cannot fetch ${url}"
    fi
}

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
        # 'lite-v*' tag. When no stable tag exists yet (early in the
        # project's life), fall back automatically to the rolling main
        # build rather than failing — operators get a working install
        # without needing to set ADOS_RELEASE_CHANNEL=main themselves.
        version="$(_fetch "https://api.github.com/repos/${GITHUB_OWNER}/${GITHUB_REPO}/releases" \
            | grep -E '"tag_name"\s*:\s*"lite-v' \
            | head -n1 \
            | sed -E 's/.*"tag_name"\s*:\s*"(lite-v[^"]+)".*/\1/')"
        if [ -z "${version}" ]; then
            log "notice: no stable lite-v* release found; using rolling lite-agent-main"
            version="lite-agent-main"
        fi
    fi

    echo "https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases/download/${version}"
}

download() {
    local url="$1" dest="$2"
    log "fetching ${url}"
    _fetch "${url}" "${dest}"
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
    # When the build still carries the placeholder key, the CI release
    # pipeline hasn't been provisioned with the real signing key yet.
    # On the rolling main channel we tolerate this — operators on
    # `lite-agent-main` are explicitly opting into the bleeding edge.
    # On stable releases (`lite-v*` tags) the placeholder check is a
    # build error: the CI workflow is supposed to substitute the real
    # public key before the tag is cut. Refuse to install rather than
    # silently degrade to SHA256-only on a stable tag the operator
    # chose specifically because they expected signed binaries.
    if [ "${MINISIGN_PUBLIC_KEY}" = "${PLACEHOLDER_KEY}" ]; then
        if [ "${RELEASE_CHANNEL}" = "main" ]; then
            log "notice: minisign public key still placeholder on rolling main"
            log "install-lite.sh — signature verification skipped for ${base}."
            log "SHA256 was verified above; the binary's integrity is checked."
            log "(this notice goes away once CI embeds the real key on stable release)"
            return 0
        fi
        die "minisign public key in install-lite.sh is still the placeholder value
this should never happen on a stable lite-v* release; the CI workflow
is supposed to substitute the real key before the tag is cut.
abort to avoid installing a binary whose Ed25519 signature cannot be
verified. set ADOS_RELEASE_CHANNEL=main to install the rolling build,
or wait for the next stable release."
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
    # Pipe through gzip explicitly so this works on both GNU tar (which
    # supports -z) and busybox tar (which does not but accepts the
    # decompressed stream on stdin via -f -).
    gzip -dc "${artifact}" | tar -x -f - -C "${workdir}"
    [ -f "${workdir}/ados-agent-lite" ] || die "extracted artifact missing ados-agent-lite binary"
    # Buildroot rootfs (Luckfox SDK class) does not pre-create
    # /usr/local/bin. Create the install dir if it's missing — `install`
    # itself does not auto-mkdir parent directories on busybox.
    install -d -m 0755 "$(dirname "${INSTALL_BIN}")"
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
    local pair_code="${1:-}"
    if [ -f "${CONFIG_PATH}" ]; then
        log "config already present at ${CONFIG_PATH}; leaving untouched"
        # If the operator supplied a new pair code on a re-install, persist
        # it via the agent CLI rather than rewriting the whole config (we
        # don't want to clobber other fields they may have edited). The CLI
        # subcommand is best-effort here; if missing, log and continue.
        if [ -n "${pair_code}" ] && [ -x "${INSTALL_BIN}" ]; then
            if "${INSTALL_BIN}" pair "${pair_code}" 2>/dev/null; then
                log "applied new pair code via ados-agent-lite pair"
            else
                log "warn: agent CLI did not accept pair code; edit ${CONFIG_PATH} manually"
            fi
        fi
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
  # mqtt_broker is populated by the pairing flow; until paired the agent
  # skips the MQTT publish loop. convex_url defaults to the Altnautica-
  # hosted relay so unpaired boards can broadcast a pairing beacon out
  # of the box. Self-hosters override this URL (and minisign-verify
  # the install path) to point at their own relay.
  mqtt_broker: ""
  mqtt_port: 8883
  mqtt_use_tls: true
  convex_url: "https://convex-site.altnautica.com"
  api_key: "${pair_code}"

api:
  bind: "127.0.0.1:8080"
EOF
    # Tighten to 0640 — file holds cloud.api_key when paired, so it
    # must not be world-readable on multi-user SBCs. Owned by root, the
    # service runs as root, no separate ados user yet.
    chmod 0640 "${CONFIG_PATH}"
    if [ -n "${pair_code}" ]; then
        log "wrote config at ${CONFIG_PATH} (device_id: ${device_id}, paired)"
    else
        log "wrote config at ${CONFIG_PATH} (device_id: ${device_id}, unpaired)"
    fi
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
    # --show-key is read-only; runs without sudo and exits before the
    # install path begins. Lets operators confirm which signing key the
    # installer trusts before they download or pair anything.
    if [ "${1:-}" = "--show-key" ]; then
        printf 'public key:  %s\n' "${MINISIGN_PUBLIC_KEY}"
        printf 'fingerprint: %s\n' "${MINISIGN_PUBLIC_KEY_FINGERPRINT}"
        return 0
    fi

    require_root

    if [ "${1:-}" = "--uninstall" ]; then
        uninstall
        return 0
    fi

    # Parse args. --pair PAIRCODE is the named form; a bare positional code
    # at the front is also accepted for back-compat. --profile and --dry-run
    # are silently swallowed because the parent install.sh consumed them
    # before exec'ing this script.
    local pair_code=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --pair)
                shift
                pair_code="${1:-}"
                if [ -z "${pair_code}" ]; then
                    die "--pair requires a CODE argument"
                fi
                shift
                ;;
            --profile)
                # Already consumed by install.sh; tolerate here so curl-pipe
                # callers that hit install-lite.sh directly with the same
                # flag don't error out.
                shift
                shift 2>/dev/null || true
                ;;
            --dry-run)
                shift
                ;;
            --uninstall)
                uninstall
                return 0
                ;;
            --upgrade)
                # --upgrade is a label: existing agent.yaml is preserved,
                # signed binary is re-fetched and replaced in place. The
                # core install path below already handles this case
                # (write_default_config skips if /etc/ados/agent.yaml
                # exists). We accept the flag explicitly so the operator
                # contract is documented and so a future sub-flow can
                # branch on it (e.g. skip the pair-code prompt).
                log "upgrade mode: existing config preserved"
                shift
                ;;
            --skip-verify)
                # Development-only escape hatch. Equivalent to setting
                # ADOS_LITE_ALLOW_UNSIGNED=1 in the environment. Loud
                # warning emitted at verify time. Default is verify-on.
                log "WARN: --skip-verify set; signature verification disabled. Do not use this in production."
                ADOS_LITE_ALLOW_UNSIGNED=1
                export ADOS_LITE_ALLOW_UNSIGNED
                shift
                ;;
            -*)
                log "warn: ignoring unknown flag: $1"
                shift
                ;;
            *)
                # Positional pair code. Same shape as the full installer
                # accepts (4-8 alphanumeric).
                if [ -z "${pair_code}" ] && [[ "$1" =~ ^[A-Za-z0-9]{4,8}$ ]]; then
                    pair_code="$1"
                else
                    log "warn: ignoring positional argument: $1"
                fi
                shift
                ;;
        esac
    done

    local target init_system release_url artifact tmpdir

    target="$(detect_target)"
    init_system="$(detect_init_system)"
    log "target architecture:  ${target}"
    log "detected init system: ${init_system}"
    if [ -n "${pair_code}" ]; then
        log "pair code provided; agent will start paired"
    else
        log "no pair code provided; agent will start unpaired and broadcast a beacon"
    fi

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
    listing="$(_fetch "${api_url}" 2>/dev/null \
        | grep -oE '"name"\s*:\s*"ados-agent-lite-[^"]+-'"${target}"'\.tar\.gz"' \
        | head -n1 \
        | sed -E 's/.*"name"\s*:\s*"([^"]+)".*/\1/')"
    if [ -z "${listing}" ]; then
        # Fallback: scrape the HTML expanded_assets endpoint.
        listing="$(_fetch "${release_url%/download/*}/expanded_assets/${release_tag}" 2>/dev/null \
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

    write_default_config "${pair_code}"

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

    print_next_steps "${pair_code}"
}

# Final user-facing message. Tells the operator what to do next based on
# whether they paired at install time or not.
print_next_steps() {
    local pair_code="${1:-}"
    local board_ip
    # Best-effort IP for the URL hint. Prefer the first non-loopback v4.
    board_ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    [ -z "${board_ip}" ] && board_ip="$(hostname 2>/dev/null).local"

    printf '\n'
    printf '====================================================================\n'
    if [ -n "${pair_code}" ]; then
        printf '  ADOS Drone Agent (lite) installed and paired\n'
        printf '====================================================================\n'
        printf '  Service:    ados-agent-lite is running\n'
        printf '  Pair code:  %s (persisted to %s)\n' "${pair_code}" "${CONFIG_PATH}"
        printf '  Webapp:     http://%s:8080/setup\n' "${board_ip}"
        printf '\n'
        printf '  The drone will appear in Mission Control within ~30 seconds.\n'
    else
        printf '  ADOS Drone Agent (lite) installed (UNPAIRED)\n'
        printf '====================================================================\n'
        printf '  Service:    ados-agent-lite is running unpaired\n'
        printf '  Webapp:     http://%s:8080/setup\n' "${board_ip}"
        printf '\n'
        printf '  To pair the drone, choose one:\n'
        printf '    1. Visit http://%s:8080/setup and complete the wizard\n' "${board_ip}"
        printf '    2. Run on this board:    sudo ados-agent-lite pair PAIRCODE\n'
        printf '    3. In Mission Control "Add drone", enter the beacon code printed\n'
        printf '       to the agent log on first boot:\n'
        printf '          sudo journalctl -u ados-agent-lite -n 50 | grep -i beacon\n'
    fi
    printf '====================================================================\n'
}

main "$@"
