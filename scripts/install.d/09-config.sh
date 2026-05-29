# shellcheck shell=bash
# =============================================================================
# 09-config.sh — device identity, default config.yaml, secret perms, pairing.
#
# Pure file-write helpers. None of these touch systemd, apt, or hardware.
# Idempotent: generate_device_id never overwrites an existing UUID and
# generate_default_config skips when config.yaml is already on disk.
# =============================================================================

# ─── Generate Device Identity ────────────────────────────────────────────────

generate_device_id() {
    # Create a stable device UUID. Once generated, never overwrite.
    if [ -f "${DEVICE_ID_FILE}" ]; then
        info "Device identity exists: $(cat "${DEVICE_ID_FILE}")"
        return
    fi

    # Store a normalized 12-char hex (no dashes) so the file format matches
    # the runtime identity helper (ados.core.identity) and the short forms
    # derived from it stay prefix-consistent.
    local device_id
    if [ -f /proc/sys/kernel/random/uuid ]; then
        device_id=$(tr -d '-' < /proc/sys/kernel/random/uuid | cut -c1-12)
    elif command -v python3 >/dev/null 2>&1; then
        device_id=$(python3 -c "import uuid; print(uuid.uuid4().hex[:12])")
    elif command -v openssl >/dev/null 2>&1; then
        device_id=$(openssl rand -hex 16 | cut -c1-12)
    else
        device_id=$(printf '%s%s%s' "$(hostname)" "$(date +%s)" "$$" | md5sum 2>/dev/null | cut -c1-12)
        [ -n "$device_id" ] || device_id=$(date +%s | tail -c 13)
    fi

    echo "$device_id" > "${DEVICE_ID_FILE}"
    chmod 644 "${DEVICE_ID_FILE}"
    info "Device identity generated: ${device_id}"
}

# ─── Reconcile Identity Into An Existing Config ─────────────────────────────

reconcile_config_identity() {
    # Inject agent.name + agent.device_id into an EXISTING config.yaml without
    # rewriting the rest of the file. Used when a prior stage wrote a partial
    # config (profile resolution writes a minimal one), or on --upgrade with
    # --name. Idempotent: never clobbers an operator-set name. Uses python so
    # YAML round-trips safely; no-ops if no python is available.
    local config_file="$1"
    local py="${VENV_DIR:-/opt/ados/venv}/bin/python"
    [ -x "$py" ] || py="$(command -v python3 || true)"
    [ -n "$py" ] || return 0

    local device_id=""
    [ -f "${DEVICE_ID_FILE}" ] && device_id=$(cat "${DEVICE_ID_FILE}")
    local short_id="${device_id:0:8}"

    DRONE_NAME="${DRONE_NAME:-}" ADOS_SHORT_ID="${short_id}" "$py" - "$config_file" <<'PY' || \
        warn "Could not reconcile identity into config.yaml; continuing."
import os, sys
from pathlib import Path
import yaml
p = Path(sys.argv[1])
cfg = {}
if p.exists():
    try:
        cfg = yaml.safe_load(p.read_text()) or {}
    except Exception:
        cfg = {}
agent = cfg.setdefault("agent", {})
short_id = os.environ.get("ADOS_SHORT_ID", "").strip()
if short_id and not agent.get("device_id"):
    agent["device_id"] = short_id
name = os.environ.get("DRONE_NAME", "").strip()
if name and agent.get("name", "") in ("", "my-drone"):
    agent["name"] = name
elif not agent.get("name") and short_id:
    agent["name"] = f"ados-{short_id}"
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(yaml.safe_dump(cfg, sort_keys=False, default_flow_style=False))
PY
    chmod 0600 "$config_file" 2>/dev/null || true
}

# ─── Generate Default Config ────────────────────────────────────────────────

generate_default_config() {
    local config_file="${CONFIG_DIR}/config.yaml"

    # If a config already exists (a minimal one written during profile
    # resolution, or an --upgrade), do NOT regenerate from scratch — only
    # reconcile the install-provided name + the stable short device_id into
    # it, then return.
    if [ -f "$config_file" ]; then
        reconcile_config_identity "$config_file"
        info "Config already exists at ${config_file}; reconciled identity fields."
        return
    fi

    info "Generating default config at ${config_file}..."

    # Read device ID (first 8 chars for agent name)
    local device_id=""
    if [ -f "${DEVICE_ID_FILE}" ]; then
        device_id=$(cat "${DEVICE_ID_FILE}")
    fi
    local short_id="${device_id:0:8}"

    # Use custom name if provided via --name flag
    local agent_name="${DRONE_NAME:-ados-${short_id}}"

    # Auto-detect FC serial port
    local fc_port=""
    for pattern in /dev/ttyACM* /dev/ttyAMA* /dev/ttyUSB*; do
        for port in $pattern; do
            if [ -e "$port" ]; then
                fc_port="$port"
                break 2
            fi
        done
    done

    if [ -n "$fc_port" ]; then
        info "Detected flight controller at: ${fc_port}"
    fi

    # Resolved agent profile. resolve_profile always emits the underscore
    # form the AgentConfig validator accepts (drone | ground_station | auto);
    # install.sh exports ADOS_PROFILE before dispatching here. If it is
    # somehow unset (manual reentry) fall back to "auto" so the agent still
    # resolves at runtime via /etc/ados/profile.conf.
    local agent_profile="${ADOS_PROFILE:-auto}"

    cat > "$config_file" <<CFGEOF
# ADOS Drone Agent Configuration
# Generated by install.sh on $(date -Iseconds 2>/dev/null || date)
# Docs: https://docs.altnautica.com/drone-agent/config

agent:
  device_id: "${short_id}"
  name: "${agent_name}"
  profile: "${agent_profile}"
  tier: "auto"

mavlink:
  serial_port: "${fc_port}"
  baud_rate: 57600
  system_id: 1
  component_id: 191

logging:
  level: "info"
  max_size_mb: 50
  keep_count: 5
  flight_log_dir: "/var/ados/logs/flights"

server:
  mode: "local"
  telemetry_rate: 2
  heartbeat_interval: 5
  mqtt_transport: "websockets"
  mqtt_username: "ados"
  mqtt_password: ""

security:
  api:
    cors_enabled: true

scripting:
  rest_api:
    enabled: true
    host: "0.0.0.0"
    port: 8080

pairing:
  convex_url: "${CONVEX_URL}"
  beacon_interval: 30
  heartbeat_interval: 60

discovery:
  mdns_enabled: true

# Video pipeline defaults. Empty cloud_relay_url means local mediamtx
# only; configure post-install when a cloud relay is ready.
video:
  mode: "auto"
  cloud_relay_url: ""
  record: false
  camera:
    width: 1280
    height: 720
    fps: 30
    codec: "h264"
    bitrate_kbps: 4000
CFGEOF

    chmod 0600 "$config_file"
    info "Default config written."
}

# ─── Harden Secret File Permissions ─────────────────────────────────────────

harden_secret_perms() {
    # Idempotent: only chmod files that exist; absence is fine on first install.
    # Tightens secrets in /etc/ados to 0600 (root-only). All ados-* services run as root.
    for f in "${CONFIG_DIR}/pairing.json" \
             "${CONFIG_DIR}/config.yaml" \
             "${CONFIG_DIR}/setup-token" \
             "${CONFIG_DIR}/env"; do
        if [ -f "$f" ]; then chmod 0600 "$f" 2>/dev/null || true; fi
    done
    if [ -d "${CONFIG_DIR}/plugin-keys" ]; then
        chmod 0700 "${CONFIG_DIR}/plugin-keys" 2>/dev/null || true
        find "${CONFIG_DIR}/plugin-keys" -maxdepth 1 -type f -name '*.pem' \
            -exec chmod 0600 {} + 2>/dev/null || true
    fi
    if [ -d "${CONFIG_DIR}/wfb" ]; then
        find "${CONFIG_DIR}/wfb" -maxdepth 1 -type f -name '*.key' \
            -exec chmod 0600 {} + 2>/dev/null || true
    fi
}

# ─── Provision First-Party Plugin Trust Keys ────────────────────────────────

provision_plugin_keys() {
    # Drop the first-party publisher public keys at /etc/ados/plugin-keys/
    # so the agent can verify signed .adosplug archives against the
    # FIRST_PARTY_SIGNERS allowlist. Keys ship inside the package at
    # scripts/plugin-keys/ and get persisted by 11-artifacts.sh to
    # /opt/ados/source/scripts/plugin-keys/ for the curl-pipe path.
    # Idempotent: overwrites are fine since the key bytes are stable
    # across reinstalls.
    local dst_dir="${CONFIG_DIR}/plugin-keys"
    install -d -m 0700 "${dst_dir}"

    local copied=0
    for src_dir in \
        "${PKG_DIR:-/opt/ados}/scripts/plugin-keys" \
        "/opt/ados/source/scripts/plugin-keys" \
    ; do
        [ -d "${src_dir}" ] || continue
        for pem in "${src_dir}"/*.pem; do
            [ -f "${pem}" ] || continue
            install -m 0600 "${pem}" "${dst_dir}/$(basename "${pem}")"
            copied=1
        done
        if [ "${copied}" -eq 1 ]; then
            return 0
        fi
    done
}

# ─── Seed Default Peripheral Manifests ──────────────────────────────────────

seed_default_peripherals() {
    # Drop the BOM peripheral manifests at /etc/ados/peripherals/ so the
    # webapp Peripherals page renders the FC, GPS, RTL8812EU adapter,
    # OLED, SPI LCD, and USB camera entries on a fresh board even
    # before any plugin lights them up.
    #
    # Idempotency rule: we overwrite a target file only when it is
    # clearly one of OUR shipped seeds (first line declares
    # ``id: ados.``). Operator-added manifests (any id outside the
    # ``ados.`` prefix, or any file the operator dropped manually)
    # are preserved. This lets us push schema corrections to the
    # default manifests without trampling operator state.
    local dst_dir="${CONFIG_DIR}/peripherals"
    install -d -m 0755 "${dst_dir}"

    local copied=0
    local refreshed=0
    for src_dir in \
        "${PKG_DIR:-/opt/ados}/scripts/peripherals-seed" \
        "/opt/ados/source/scripts/peripherals-seed" \
    ; do
        [ -d "${src_dir}" ] || continue
        for manifest in "${src_dir}"/*.yaml; do
            [ -f "${manifest}" ] || continue
            local target
            target="${dst_dir}/$(basename "${manifest}")"
            if [ ! -f "${target}" ]; then
                install -m 0644 "${manifest}" "${target}"
                copied=$((copied + 1))
            elif grep -q "^id:[[:space:]]*ados\." "${target}" 2>/dev/null; then
                if ! cmp -s "${manifest}" "${target}"; then
                    install -m 0644 "${manifest}" "${target}"
                    refreshed=$((refreshed + 1))
                fi
            fi
        done
        if [ "${copied}" -gt 0 ] || [ "${refreshed}" -gt 0 ]; then
            info "Peripheral manifests: seeded ${copied}, refreshed ${refreshed}"
            return 0
        fi
    done
}

# ─── Write Pairing State ────────────────────────────────────────────────────

write_pairing() {
    local code="$1"
    local pairing_file="${CONFIG_DIR}/pairing.json"
    local code_upper
    code_upper=$(echo "$code" | tr '[:lower:]' '[:upper:]')

    info "Setting pairing code: ${code_upper}"
    cat > "$pairing_file" <<PAIREOF
{
  "pairing_code": "${code_upper}",
  "code_created_at": $(date +%s)
}
PAIREOF
    chmod 0600 "$pairing_file"
}

# ─── Set System Hostname From --name ────────────────────────────────────────

set_hostname() {
    # Only act when the operator passed --name. Slugify to a DNS-safe label
    # (lowercase, [a-z0-9-] only, collapsed, trimmed, max 63 chars) and set the
    # system hostname so <slug>.local resolves via avahi alongside the agent's
    # own ados-<id>.local advertisement. Idempotent: no-op if already set.
    [ -n "${DRONE_NAME:-}" ] || return 0

    local slug
    slug=$(printf '%s' "${DRONE_NAME}" \
        | tr '[:upper:]' '[:lower:]' \
        | sed -E 's/[^a-z0-9-]+/-/g; s/-+/-/g; s/^-+//; s/-+$//' \
        | cut -c1-63)
    slug=$(printf '%s' "$slug" | sed -E 's/-+$//')
    [ -n "$slug" ] || { warn "--name '${DRONE_NAME}' slugified to empty; leaving hostname unchanged."; return 0; }

    local current
    current=$(hostname 2>/dev/null || cat /etc/hostname 2>/dev/null || echo "")
    if [ "$current" = "$slug" ]; then
        info "Hostname already '${slug}'."
        return 0
    fi

    if command -v hostnamectl >/dev/null 2>&1; then
        if hostnamectl set-hostname "$slug" 2>/dev/null; then
            info "Hostname set to '${slug}'."
        else
            warn "hostnamectl failed; falling back to /etc/hostname."
            printf '%s\n' "$slug" > /etc/hostname 2>/dev/null || true
        fi
    else
        printf '%s\n' "$slug" > /etc/hostname 2>/dev/null || true
        hostname "$slug" 2>/dev/null || true
        info "Hostname set to '${slug}' (no hostnamectl)."
    fi

    # Keep the /etc/hosts 127.0.1.1 entry consistent so name resolution does
    # not stall. Idempotent rewrite of that one line only.
    if [ -f /etc/hosts ]; then
        if grep -qE '^127\.0\.1\.1[[:space:]]' /etc/hosts; then
            sed -i -E "s/^127\.0\.1\.1[[:space:]].*/127.0.1.1\t${slug}/" /etc/hosts 2>/dev/null || true
        else
            printf '127.0.1.1\t%s\n' "$slug" >> /etc/hosts 2>/dev/null || true
        fi
    fi

    # Reconcile mDNS so <slug>.local resolves. Best-effort.
    if command -v systemctl >/dev/null 2>&1; then
        systemctl restart avahi-daemon 2>/dev/null || true
    fi
}
