# shellcheck shell=bash
# =============================================================================
# 00-detect.sh — host platform, profile, and Python detection helpers.
#
# Pure read-only inspection of /proc, /etc/os-release, uname, and the
# agent venv. detect_profile() is called by the dispatcher pre-parse
# block to decide between the Python full agent and the Rust lite agent.
# resolve_profile() runs later, after the agent venv is built, to read
# the canonical agent.profile from disk + flags.
# =============================================================================

# Profile dispatch: walk /proc/device-tree/model, /proc/cpuinfo, and
# /proc/meminfo against the lite-eligible board manifest. Returns
# "lite-rs" when the board matches or RAM <= LITE_RAM_FAILSAFE_KB,
# "full" otherwise.
detect_profile() {
    local model="" model_lower="" mem_kb=""

    # Primary fingerprint: /proc/device-tree/model. Strip the null byte
    # the kernel terminates the string with.
    if [ -r /proc/device-tree/model ]; then
        model="$(tr -d '\000' < /proc/device-tree/model 2>/dev/null || true)"
    fi

    # Fallback: /proc/cpuinfo "Hardware" line (older Pi kernels, x86 has no
    # device-tree at all).
    if [ -z "${model}" ] && [ -r /proc/cpuinfo ]; then
        model="$(awk -F: '/^Hardware/ {sub(/^ */, "", $2); print $2; exit}' /proc/cpuinfo)"
    fi

    if [ -n "${model}" ]; then
        model_lower="$(printf '%s' "${model}" | tr '[:upper:]' '[:lower:]')"
    fi

    # Fetch the lite-eligible board manifest. 5s timeout so a network blip
    # doesn't stall every install. Failure falls through to the RAM failsafe.
    # Prefers curl, falls back to wget so this works on Buildroot rootfs
    # images (Luckfox SDK, etc.) that ship wget but not curl.
    local manifest=""
    if command -v curl >/dev/null 2>&1; then
        manifest="$(curl -fsSL --max-time 5 "${LITE_BOARDS_MANIFEST_URL}" 2>/dev/null || true)"
    elif command -v wget >/dev/null 2>&1; then
        manifest="$(wget -q -T 5 -O - "${LITE_BOARDS_MANIFEST_URL}" 2>/dev/null || true)"
    fi

    if [ -n "${manifest}" ] && [ -n "${model_lower}" ]; then
        # Extract every model_pattern from the manifest. Use python3 (always
        # present on a fresh BSP); jq is faster but not always installed.
        local patterns=""
        if command -v python3 >/dev/null 2>&1; then
            patterns="$(printf '%s' "${manifest}" | python3 -c '
import json, sys
try:
    m = json.load(sys.stdin)
    for b in m.get("boards", []):
        for p in b.get("model_patterns", []) or []:
            print(p)
except Exception:
    pass
' 2>/dev/null || true)"
        elif command -v jq >/dev/null 2>&1; then
            patterns="$(printf '%s' "${manifest}" | jq -r '.boards[]?.model_patterns[]?' 2>/dev/null || true)"
        fi

        if [ -n "${patterns}" ]; then
            local pattern pattern_lower
            while IFS= read -r pattern; do
                [ -z "${pattern}" ] && continue
                pattern_lower="$(printf '%s' "${pattern}" | tr '[:upper:]' '[:lower:]')"
                case "${model_lower}" in
                    *"${pattern_lower}"*)
                        echo "lite-rs"
                        return 0
                        ;;
                esac
            done <<< "${patterns}"
        fi
    fi

    # Failsafe: any board with <= 384 MB total RAM gets the lite path even
    # when the manifest fetch failed or didn't list it.
    if [ -r /proc/meminfo ]; then
        mem_kb="$(awk '/^MemTotal:/ {print $2; exit}' /proc/meminfo 2>/dev/null || true)"
        if [ -n "${mem_kb}" ] && [ "${mem_kb}" -le "${LITE_RAM_FAILSAFE_KB}" ]; then
            echo "lite-rs"
            return 0
        fi
    fi

    echo "full"
}

# ─── Architecture Detection ─────────────────────────────────────────────────

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        aarch64|arm64)  echo "aarch64" ;;
        armv7l|armhf)   echo "armhf" ;;
        x86_64|amd64)   echo "x86_64" ;;
        *)              echo "$arch" ;;
    esac
}

# ─── OS Detection ───────────────────────────────────────────────────────────

detect_os() {
    # Returns: raspbian, ubuntu, armbian, debian, darwin, unknown
    local os_name="unknown"

    if [ "$(uname -s)" = "Darwin" ]; then
        echo "darwin"
        return
    fi

    if [ -f /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        case "${ID:-}" in
            raspbian)   os_name="raspbian" ;;
            ubuntu)     os_name="ubuntu" ;;
            armbian)    os_name="armbian" ;;
            debian)     os_name="debian" ;;
            *)          os_name="${ID:-unknown}" ;;
        esac
    fi
    echo "$os_name"
}

# ─── Python Detection ───────────────────────────────────────────────────────

find_python() {
    # Finds the best available Python 3.11+ binary
    for py in python3.13 python3.12 python3.11 python3; do
        if command -v "$py" &>/dev/null; then
            local ver major minor
            ver=$("$py" -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
            major=$(echo "$ver" | cut -d. -f1)
            minor=$(echo "$ver" | cut -d. -f2)
            if [ "$major" -ge 3 ] && [ "$minor" -ge 11 ]; then
                echo "$py"
                return
            fi
        fi
    done
    echo ""
}

# ─── Agent Profile Resolution ───────────────────────────────────────────────

# Resolve agent profile. Priority:
#
#   1. --profile flag (from the pre-parser in the dispatcher). Persists
#      to /etc/ados/profile.conf AND updates /etc/ados/config.yaml's
#      agent.profile + ground_station block so the choice survives
#      reboots and so the agent itself reports the right profile via
#      its REST API.
#   2. /etc/ados/profile.conf — supports both YAML (`profile: X`) and
#      legacy key=value (`profile=X`) so older installs don't break.
#   3. python -m ados.bootstrap.profile_detect — auto-detect from
#      board fingerprint. Always returns a usable value.
#   4. Fallback: "drone".
#
# A stale "unconfigured" written by an older agent fails the regex
# below and falls through to priority 3 so the install self-heals.
resolve_profile() {
    local profile_file="${CONFIG_DIR}/profile.conf"
    local valid_re='^(auto|drone|ground_station|ground-station)$'

    # Priority 1 — explicit --profile flag (already in _PROFILE_OVERRIDE).
    if [ -n "${_PROFILE_OVERRIDE:-}" ] \
        && [ "${_PROFILE_OVERRIDE}" != "auto" ] \
        && [[ "${_PROFILE_OVERRIDE}" =~ ${valid_re} ]]; then
        # Normalize "ground-station" → "ground_station" for the on-disk
        # canonical form. The agent's setup contract uses the
        # underscore form everywhere internally; install.sh accepts
        # both for ergonomics.
        local normalized="${_PROFILE_OVERRIDE//-/_}"
        mkdir -p "${CONFIG_DIR}"
        cat > "${profile_file}" <<EOF
profile: ${normalized}
EOF
        # Push the same value into config.yaml's agent.profile so the
        # running agent reports it through the REST API. ground_station
        # role defaults to "direct" — operator can change later via the
        # wizard's profile step.
        _persist_profile_to_config "${normalized}"
        echo "${normalized}"
        return 0
    fi

    # Priority 2 — on-disk profile.conf. Try YAML first, then legacy.
    if [ -f "${profile_file}" ]; then
        local val
        # YAML form: `profile: X`
        val="$(grep -E '^profile:[[:space:]]+' "${profile_file}" 2>/dev/null \
            | head -n1 | sed -E 's/^profile:[[:space:]]+//' | tr -d '[:space:]' || true)"
        if [ -z "${val}" ]; then
            # Legacy key=value form: `profile=X`
            val="$(grep -E '^profile=' "${profile_file}" 2>/dev/null \
                | cut -d= -f2 | tr -d '[:space:]' || true)"
        fi
        if [[ "${val}" =~ ${valid_re} ]]; then
            local normalized="${val//-/_}"
            echo "${normalized}"
            return 0
        fi
        warn "Ignoring unrecognized profile.conf contents."
    fi

    # Priority 3 — auto-detect via the agent's profile_detect. Stderr is
    # captured to a tmp file (not /dev/null) so an import error or an
    # exception in detect_profile() lands in the install log instead of
    # silently letting us fall through to the drone default. The earlier
    # silent-fallthrough is what put a freshly-purged ground-station box
    # on the wrong profile branch during install, which then skipped the
    # LCD overlay step before the post-install runtime probe corrected
    # the profile (LCD never got provisioned that cycle).
    local detect_stderr; detect_stderr="$(mktemp)"
    local detect_rc=0
    local detected=""
    # detect_profile() emits a structlog "profile_detect_result" line
    # via log.info(...) before our print() runs. Default structlog
    # writes to stdout, which would concat the log line with the
    # profile value if we grabbed the whole capture. The python
    # snippet below silences structlog explicitly AND we still take
    # the last line as a defence-in-depth measure for future log
    # additions inside the detect path.
    local py_snippet='
import logging, sys
logging.disable(logging.CRITICAL)
try:
    import structlog
    structlog.configure(wrapper_class=structlog.make_filtering_bound_logger(logging.CRITICAL + 10))
except Exception:
    pass
from ados.bootstrap.profile_detect import detect_profile
sys.stdout.write(detect_profile()["profile"])
'
    if "${VENV_DIR}/bin/python" -c "import ados.bootstrap.profile_detect" 2>"${detect_stderr}"; then
        detected="$("${VENV_DIR}/bin/python" -c "${py_snippet}" 2>"${detect_stderr}" | tail -n 1)" || detect_rc=$?
        detected="$(echo "${detected}" | tr -d '[:space:]')"
    else
        detect_rc=$?
    fi
    if [ "${detect_rc}" -ne 0 ] || [ -z "${detected}" ]; then
        local stderr_head
        stderr_head="$(head -c 400 "${detect_stderr}" 2>/dev/null | tr '\n' ' ')"
        warn "Profile auto-detect failed (rc=${detect_rc}); falling back to drone. stderr: ${stderr_head:-<empty>}"
    elif [[ "${detected}" =~ ${valid_re} ]]; then
        rm -f "${detect_stderr}"
        mkdir -p "${CONFIG_DIR}"
        cat > "${profile_file}" <<EOF
profile: ${detected//-/_}
EOF
        echo "${detected//-/_}"
        return 0
    else
        warn "Profile auto-detect returned an unrecognised value '${detected}'; falling back to drone."
    fi
    rm -f "${detect_stderr}"

    # Priority 4 — fallback.
    echo "drone"
}

# Persist agent.profile (and a default ground_station block when
# applicable) into /etc/ados/config.yaml. Uses python because YAML
# editing in pure shell is fragile. Idempotent — overwrites the
# field rather than appending.
_persist_profile_to_config() {
    local target_profile="$1"
    local config_file="${CONFIG_DIR}/config.yaml"
    if [ ! -x "${VENV_DIR}/bin/python" ]; then
        # Venv not built yet (very early --force install). Skip; the
        # profile will be re-applied on the post-install resolve.
        return 0
    fi
    "${VENV_DIR}/bin/python" - "${config_file}" "${target_profile}" <<'PY' || \
        warn "Could not update config.yaml agent.profile; do it via the wizard."
import sys
from pathlib import Path
import yaml  # type: ignore[import-not-found]

cfg_path = Path(sys.argv[1])
target = sys.argv[2]

cfg = {}
if cfg_path.exists():
    try:
        cfg = yaml.safe_load(cfg_path.read_text()) or {}
    except Exception:
        cfg = {}

agent = cfg.setdefault("agent", {})
agent["profile"] = target

# When promoting to ground_station, seed a default role so the agent
# has something to work with before the operator opens the wizard.
# Idempotent — never overwrites a role the operator has already set.
if target == "ground_station":
    gs = cfg.setdefault("ground_station", {})
    gs.setdefault("role", "direct")
    gs.setdefault("mesh_capable", False)

cfg_path.parent.mkdir(parents=True, exist_ok=True)
cfg_path.write_text(yaml.safe_dump(cfg, sort_keys=False, default_flow_style=False))
PY
}
