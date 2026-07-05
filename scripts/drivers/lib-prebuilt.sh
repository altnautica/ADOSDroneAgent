#!/usr/bin/env bash
# lib-prebuilt.sh — sourceable helper that installs a verified PREBUILT
# RTL8812EU kernel module matched to the running kernel, so a fresh install
# does not have to compile the driver on the device. On-device DKMS builds
# are slow and, on marginal hardware, can crash the compiler outright; a
# prebuilt module sidesteps that for the kernels we publish.
#
# try_prebuilt_install MODULE KVER KARCH
#   1. fetch + verify the manifest (drivers-manifest.json)
#   2. match (module, kver, arch) in the manifest
#   3. download + verify the matching .ko (SHA256 mandatory; signature
#      optional while ADOS_PREBUILT_ALLOW_UNSIGNED=1 — the dev/test default)
#   4. vermagic strict-compare against the running kernel
#   5. install to /lib/modules/<kver>/updates/, depmod, modprobe, confirm
#   returns 0 when the module is loaded; non-zero tells the caller to fall
#   back to the DKMS build. Never exits the shell.
#
# Env:
#   ADOS_DRIVER_PREBUILT=0          skip the prebuilt path entirely (force DKMS)
#   ADOS_PREBUILT_BASE_URL=<url>    override the release base URL (testing)
#   ADOS_PREBUILT_ALLOW_UNSIGNED=1  accept a SHA256-only artifact (dev default);
#                                   set 0 to require a valid signature (prod)
#   ADOS_PREBUILT_VERMAGIC_STRICT=1 require an exact vermagic match (default)

_LIBPB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_LIBPB_REPO_ROOT="$(cd "${_LIBPB_DIR}/../.." && pwd)"

# Minimal loggers when the caller has not provided richer ones.
command -v info >/dev/null 2>&1 || info() { printf '[INFO]  %s\n' "$*" >&2; }
command -v warn >/dev/null 2>&1 || warn() { printf '[WARN]  %s\n' "$*" >&2; }

# Shared fetch + verify helpers. Re-sourcing is safe (idempotent).
# shellcheck source=scripts/lib/net.sh disable=SC1091
. "${_LIBPB_REPO_ROOT}/scripts/lib/net.sh"
# shellcheck source=scripts/lib/verify.sh disable=SC1091
. "${_LIBPB_REPO_ROOT}/scripts/lib/verify.sh"

# Default release base. The 'latest' redirect points at the newest stable
# drivers-v* release; the rolling 'prebuilt-drivers' prerelease is the dev
# default base when no stable release exists yet.
ADOS_PREBUILT_BASE_URL="${ADOS_PREBUILT_BASE_URL:-https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-drivers}"

# minisign public-key STRING (verify.sh passes it to `minisign -P`, so this is
# the key itself, not a file path). Defaults to the vendored public half of the
# ADOS_DRIVER_SIGNING_KEY keypair; an operator can override it with their own
# key, and ADOS_PREBUILT_ALLOW_UNSIGNED=1 forces SHA256-only. When a signed
# .minisig exists (CI signs once the secret is set) it is verified against this.
_pb_pubkey() {
    printf '%s\n' "${ADOS_DRIVER_PUBKEY:-RWQ/CJ1+gk7rjVfGSoy6MOL50e8TmO30KD/J+goaEj+WMI1uzEf92rHN}"
}

# Running kernel's vermagic, read from a currently-loaded in-tree module so we
# compare against what THIS kernel actually accepts (version + SMP + preempt +
# arch flags), not a guessed string. Empty when none can be read.
_pb_running_vermagic() {
    local mod vm
    for mod in cfg80211 mac80211 nvme ip_tables; do
        vm="$(modinfo -F vermagic "${mod}" 2>/dev/null || true)"
        [ -n "${vm}" ] && { printf '%s\n' "${vm}"; return 0; }
    done
    # Fall back to the first loaded module that resolves a vermagic.
    local m
    for m in $(lsmod 2>/dev/null | awk 'NR>1{print $1}'); do
        vm="$(modinfo -F vermagic "${m}" 2>/dev/null || true)"
        [ -n "${vm}" ] && { printf '%s\n' "${vm}"; return 0; }
    done
    return 1
}

# Match (module, kver, arch) in the manifest; echo "file<TAB>sha256<TAB>vermagic"
# for the row, or return non-zero. Uses python3 (present after stage-1 deps).
_pb_manifest_match() {
    local manifest="$1" module="$2" kver="$3" karch="$4"
    command -v python3 >/dev/null 2>&1 || return 1
    MOD="${module}" KVER="${kver}" KARCH="${karch}" python3 - "${manifest}" <<'PY'
import json, os, sys
mod, kver, karch = os.environ["MOD"], os.environ["KVER"], os.environ["KARCH"]
try:
    with open(sys.argv[1]) as f:
        data = json.load(f)
except Exception:
    sys.exit(1)
for row in data.get("drivers", []):
    if row.get("module") == mod and row.get("kver") == kver and row.get("arch") == karch:
        print("\t".join([str(row.get("file", "")), str(row.get("sha256", "")), str(row.get("vermagic", ""))]))
        sys.exit(0)
sys.exit(1)
PY
}

try_prebuilt_install() {
    local module="$1" kver="$2" karch="$3"
    [ "${ADOS_DRIVER_PREBUILT:-1}" = "1" ] || return 1
    [ -n "${module}" ] && [ -n "${kver}" ] && [ -n "${karch}" ] || return 1
    command -v modprobe >/dev/null 2>&1 || return 1

    local base="${ADOS_PREBUILT_BASE_URL}"
    local allow_unsigned="${ADOS_PREBUILT_ALLOW_UNSIGNED:-1}"
    local pubkey; pubkey="$(_pb_pubkey)"
    local tmp; tmp="$(mktemp -d)" || return 1

    # 1. manifest (+ its sidecar SHA256/signature, verified like any artifact).
    if ! ados_fetch "${base}/drivers-manifest.json" "${tmp}/drivers-manifest.json" 20; then
        info "No prebuilt driver manifest reachable; will build from source."
        rm -rf "${tmp}"; return 1
    fi
    ados_fetch "${base}/drivers-manifest.json.sha256" "${tmp}/drivers-manifest.json.sha256" 15 2>/dev/null || true
    ados_fetch "${base}/drivers-manifest.json.minisig" "${tmp}/drivers-manifest.json.minisig" 15 2>/dev/null || true
    if [ -f "${tmp}/drivers-manifest.json.sha256" ]; then
        ados_verify_artifact "${tmp}/drivers-manifest.json" "${pubkey}" "prebuilt" "${allow_unsigned}" \
            || { warn "prebuilt manifest failed verification; building from source."; rm -rf "${tmp}"; return 1; }
    fi

    # 2. match this exact kernel + arch.
    local row file sha vermagic
    row="$(_pb_manifest_match "${tmp}/drivers-manifest.json" "${module}" "${kver}" "${karch}")" || {
        info "No prebuilt ${module} for ${kver}/${karch}; building from source."
        rm -rf "${tmp}"; return 1
    }
    file="$(printf '%s' "${row}" | cut -f1)"
    sha="$(printf '%s' "${row}" | cut -f2)"
    vermagic="$(printf '%s' "${row}" | cut -f3)"
    [ -n "${file}" ] || { rm -rf "${tmp}"; return 1; }

    # 3. download the .ko + its sidecars.
    if ! ados_fetch "${base}/${file}" "${tmp}/${file}" 60; then
        warn "prebuilt ${file} download failed; building from source."
        rm -rf "${tmp}"; return 1
    fi
    # Prefer the published sha256 sidecar; synthesize one from the manifest
    # hash when the sidecar is absent so ados_verify_sha256 always has input.
    if ! ados_fetch "${base}/${file}.sha256" "${tmp}/${file}.sha256" 15 2>/dev/null; then
        [ -n "${sha}" ] && printf '%s  %s\n' "${sha}" "${file}" > "${tmp}/${file}.sha256"
    fi
    ados_fetch "${base}/${file}.minisig" "${tmp}/${file}.minisig" 15 2>/dev/null || true

    # 4. verify: SHA256 mandatory, signature per the dev/prod posture.
    if ! ados_verify_artifact "${tmp}/${file}" "${pubkey}" "prebuilt" "${allow_unsigned}"; then
        warn "prebuilt ${file} failed verification; building from source."
        rm -rf "${tmp}"; return 1
    fi

    # 5. vermagic strict-compare against the running kernel.
    if [ "${ADOS_PREBUILT_VERMAGIC_STRICT:-1}" = "1" ]; then
        local ko_vm running_vm
        ko_vm="$(modinfo -F vermagic "${tmp}/${file}" 2>/dev/null || true)"
        running_vm="$(_pb_running_vermagic || true)"
        if [ -n "${running_vm}" ] && [ -n "${ko_vm}" ] && [ "${ko_vm}" != "${running_vm}" ]; then
            warn "prebuilt vermagic '${ko_vm}' != running '${running_vm}'; building from source."
            rm -rf "${tmp}"; return 1
        fi
        # Manifest's declared vermagic should agree with the .ko's own.
        if [ -n "${vermagic}" ] && [ -n "${ko_vm}" ] && [ "${vermagic}" != "${ko_vm}" ]; then
            warn "prebuilt manifest vermagic disagrees with the module; building from source."
            rm -rf "${tmp}"; return 1
        fi
    fi

    # 6. install + load + confirm.
    install -d -m 0755 "/lib/modules/${kver}/updates" 2>/dev/null || true
    if ! install -m 0644 "${tmp}/${file}" "/lib/modules/${kver}/updates/${module}.ko"; then
        warn "could not place prebuilt module; building from source."
        rm -rf "${tmp}"; return 1
    fi
    rm -rf "${tmp}"
    depmod -a "${kver}" 2>/dev/null || true
    if ! modprobe "${module}" 2>/dev/null; then
        warn "prebuilt module failed to load; building from source."
        return 1
    fi
    if ! lsmod | awk '{print $1}' | grep -qx "${module}"; then
        warn "prebuilt module not resident after modprobe; building from source."
        return 1
    fi
    mkdir -p /run/ados 2>/dev/null || true
    printf 'prebuilt\n' > /run/ados/wfb-module-source 2>/dev/null || true
    return 0
}
