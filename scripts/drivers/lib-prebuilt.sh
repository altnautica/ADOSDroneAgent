# shellcheck shell=bash
# =============================================================================
# lib-prebuilt.sh — prebuilt RTL8812EU kernel-module install path.
#
# Sourceable library. Defines try_prebuilt_install, which attempts to load
# a verified prebuilt 8812eu.ko for the running kernel instead of compiling
# from scratch via DKMS. It NEVER exits — every failure path returns a
# non-zero status and cleans up, so the caller's `set -e` cannot abort
# before the universal DKMS fallback runs.
#
# Flow of try_prebuilt_install MODULE KVER KARCH:
#   1. offline guard (ados_reachable) — skip fast when off-internet
#   2. fetch + verify the signed drivers-manifest.json
#   3. look up (module, kver, arch) in the manifest
#   4. download + verify the matching .ko (SHA256 + minisign)
#   5. vermagic safety check against the running kernel
#   6. install to /lib/modules/<kver>/updates/, depmod, modprobe, lsmod
#   7. write the /run/ados/wfb-module-source breadcrumb = "prebuilt"
#
# Knobs (env):
#   ADOS_DRIVER_PREBUILT          1 (default) try prebuilt; 0 forces DKMS
#                                 (read by install-rtl8812eu.sh, not here)
#   ADOS_PREBUILT_BASE_URL        override the manifest base URL (testing)
#   ADOS_PREBUILT_VERMAGIC_STRICT 1 (default) full vermagic compare;
#                                 0 relaxes to kernel-version-token match
#   ADOS_PREBUILT_ALLOW_UNSIGNED  passed through to ados_verify_artifact.
#                                 Skips ONLY the minisign signature check; the
#                                 SHA256 checksum is ALWAYS still enforced.
#                                 Ignored on the stable channel (signatures
#                                 there are mandatory).
# =============================================================================

# Resolve this script's dir + repo root so we can source the net/verify libs.
_LIBPB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_LIBPB_REPO_ROOT="$(cd "${_LIBPB_DIR}/../.." && pwd)"

# Plain-loggers fallback when the caller has not provided richer ones.
command -v info  >/dev/null 2>&1 || info()  { printf '[prebuilt] %s\n' "$*" >&2; }
command -v warn  >/dev/null 2>&1 || warn()  { printf '[prebuilt] %s\n' "$*" >&2; }
command -v error >/dev/null 2>&1 || error() { printf '[prebuilt] %s\n' "$*" >&2; }

# Pull in the shared fetch + verify helpers (idempotent re-source is safe;
# they only define functions). shellcheck cannot follow the runtime path.
# shellcheck source=scripts/lib/net.sh disable=SC1091
. "${_LIBPB_REPO_ROOT}/scripts/lib/net.sh"
# shellcheck source=scripts/lib/verify.sh disable=SC1091
. "${_LIBPB_REPO_ROOT}/scripts/lib/verify.sh"

# Base URL of the rolling prebuilt-drivers prerelease. CI substitutes the
# pinned per-tag URL the same way install-lite.sh substitutes its public
# key; until then the rolling prerelease assets are the source of truth.
ADOS_PREBUILT_BASE_URL="${ADOS_PREBUILT_BASE_URL:-https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-drivers}"

# Vendored Ed25519 public key for prebuilt-module verification. The CI
# release pipeline replaces this string with the real public key on tag,
# exactly like install-lite.sh. No env override on purpose — a runtime key
# override would let a hostile environment substitute its own signing key.
ADOS_PREBUILT_PUBKEY="RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8"

# The placeholder value above. While the key is still the placeholder we
# treat artifacts as edge-channel (SHA256 mandatory, signature tolerated
# when unverifiable) so the path works before the key is provisioned.
_ADOS_PREBUILT_PLACEHOLDER="RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8"

# Find a vermagic from a currently-loaded in-tree kernel module, to compare
# against the downloaded .ko. Picks any loaded module that is not our own
# target and asks modinfo for its vermagic. Returns the vermagic on stdout,
# empty when none can be determined.
_running_kernel_vermagic() {
    local target="$1" mod vm
    command -v modinfo >/dev/null 2>&1 || return 0
    # Prefer cheap, near-universal modules; fall back to scanning lsmod.
    for mod in cfg80211 mac80211 ext4 nvme usbcore; do
        if lsmod 2>/dev/null | awk '{print $1}' | grep -qx "${mod}"; then
            vm="$(modinfo -F vermagic "${mod}" 2>/dev/null || true)"
            [ -n "${vm}" ] && { printf '%s' "${vm}"; return 0; }
        fi
    done
    while read -r mod _; do
        [ "${mod}" = "Module" ] && continue
        [ "${mod}" = "${target}" ] && continue
        vm="$(modinfo -F vermagic "${mod}" 2>/dev/null || true)"
        [ -n "${vm}" ] && { printf '%s' "${vm}"; return 0; }
    done < <(lsmod 2>/dev/null)
    return 0
}

# Compare a downloaded module's vermagic to the running kernel. STRICT
# (default) requires a full string match; relaxed mode requires only that
# the leading kernel-version token agrees. Falls back to the manifest's
# signed vermagic for the running kernel when no loaded module yields one.
# Returns 0 when safe to load, non-zero otherwise.
_vermagic_ok() {
    local ko="$1" manifest_vermagic="$2"
    local strict="${ADOS_PREBUILT_VERMAGIC_STRICT:-1}"
    local ko_vm ref_vm

    command -v modinfo >/dev/null 2>&1 || {
        warn "modinfo unavailable; cannot run vermagic safety check"
        return 1
    }
    ko_vm="$(modinfo -F vermagic "${ko}" 2>/dev/null || true)"
    if [ -z "${ko_vm}" ]; then
        warn "could not read vermagic from downloaded module"
        return 1
    fi

    ref_vm="$(_running_kernel_vermagic 8812eu)"
    if [ -z "${ref_vm}" ]; then
        # No loaded module to compare against — trust the signed manifest
        # vermagic, which was verified by the signature check upstream.
        ref_vm="${manifest_vermagic}"
    fi
    if [ -z "${ref_vm}" ]; then
        warn "no reference vermagic available for safety check"
        return 1
    fi

    if [ "${strict}" = "1" ]; then
        if [ "${ko_vm}" = "${ref_vm}" ]; then
            return 0
        fi
        warn "vermagic mismatch (strict): module='${ko_vm}' running='${ref_vm}'"
        return 1
    fi

    # Relaxed: compare only the leading kernel-version token.
    if [ "${ko_vm%% *}" = "${ref_vm%% *}" ]; then
        return 0
    fi
    warn "vermagic kernel-version mismatch: module='${ko_vm%% *}' running='${ref_vm%% *}'"
    return 1
}

# Look up a module entry's filename + sha256 in the manifest for the given
# (module, kver, arch). Prints "filename<TAB>sha256<TAB>vermagic" on stdout,
# returns non-zero when not found. Uses python3 when available, with a
# jq-free awk fallback for minimal rootfs.
_manifest_lookup() {
    local manifest="$1" module="$2" kver="$3" arch="$4"
    if command -v python3 >/dev/null 2>&1; then
        python3 - "${manifest}" "${module}" "${kver}" "${arch}" <<'PY'
import json, sys
manifest, module, kver, arch = sys.argv[1:5]
try:
    with open(manifest) as fp:
        data = json.load(fp)
except (OSError, ValueError):
    sys.exit(1)
if data.get("module") != module:
    sys.exit(1)
for m in data.get("modules", []):
    if m.get("kernelrelease") == kver and m.get("arch") == arch:
        print("\t".join([m.get("filename", ""), m.get("sha256", ""), m.get("vermagic", "")]))
        sys.exit(0)
sys.exit(1)
PY
        return $?
    fi
    # awk fallback: flatten the manifest objects and match the trio. The
    # manifest is emitted with indent=2 so each field is on its own line.
    awk -v module="${module}" -v kver="${kver}" -v arch="${arch}" '
        function val(s,   v) { v=s; sub(/^[^:]*:[[:space:]]*/,"",v); gsub(/[",]/,"",v); return v }
        /"module"[[:space:]]*:/ && !in_modules { top_module=val($0) }
        /"modules"[[:space:]]*:/ { in_modules=1 }
        in_modules && /\{/ { inobj=1; f=""; s=""; v=""; krel=""; a="" }
        in_modules && /"kernelrelease"[[:space:]]*:/ { krel=val($0) }
        in_modules && /"arch"[[:space:]]*:/ { a=val($0) }
        in_modules && /"vermagic"[[:space:]]*:/ { v=val($0) }
        in_modules && /"filename"[[:space:]]*:/ { f=val($0) }
        in_modules && /"sha256"[[:space:]]*:/ { s=val($0) }
        in_modules && /\}/ {
            if (inobj && krel==kver && a==arch && top_module==module && f!="") {
                printf "%s\t%s\t%s\n", f, s, v
                found=1
                exit 0
            }
            inobj=0
        }
        END { if (!found) exit 1 }
    ' "${manifest}"
}

# Attempt to install + load a prebuilt module. Returns 0 on success
# (installed + loaded + breadcrumb written), non-zero on any miss so the
# caller falls through to DKMS. Never exits.
try_prebuilt_install() {
    local module="$1" kver="$2" karch="$3"
    local tmp channel pubkey
    local manifest_url manifest entry filename sha256 vermagic
    local ko_url ko_path dest

    # v1 prebuilt target is arm64 only. Everything else falls to DKMS.
    if [ "${karch}" != "arm64" ]; then
        info "prebuilt path supports arm64 only; falling back to DKMS for arch '${karch}'."
        return 1
    fi

    # Offline guard — fail fast instead of hanging on a dead network.
    if ! ados_reachable "https://github.com"; then
        info "github.com not reachable; skipping prebuilt path (will use DKMS)."
        return 1
    fi

    tmp="$(mktemp -d)" || return 1
    # Clean up the scratch dir on every exit point via a RETURN trap.
    trap 'rm -rf "${tmp}"' RETURN

    # Channel selection mirrors install-lite.sh: while the vendored key is
    # still the placeholder, run as edge so SHA256-only is acceptable; once
    # CI substitutes the real key, run as stable so a signature is required.
    if [ "${ADOS_PREBUILT_PUBKEY}" = "${_ADOS_PREBUILT_PLACEHOLDER}" ]; then
        channel="edge"
        pubkey=""
    else
        channel="stable"
        pubkey="${ADOS_PREBUILT_PUBKEY}"
    fi

    # 1. Fetch + verify the manifest.
    manifest_url="${ADOS_PREBUILT_BASE_URL}/drivers-manifest.json"
    manifest="${tmp}/drivers-manifest.json"
    if ! ados_fetch "${manifest_url}" "${manifest}"; then
        warn "could not fetch prebuilt driver manifest; falling back to DKMS."
        return 1
    fi
    # The manifest itself is signed + sha256'd alongside the .ko assets.
    if ! ados_fetch "${manifest_url}.sha256" "${manifest}.sha256"; then
        warn "manifest sha256 sidecar missing; falling back to DKMS."
        return 1
    fi
    ados_fetch "${manifest_url}.minisig" "${manifest}.minisig" || true
    if ! ados_verify_artifact "${manifest}" "${pubkey}" "${channel}" \
            "${ADOS_PREBUILT_ALLOW_UNSIGNED:-0}"; then
        warn "prebuilt manifest failed verification; falling back to DKMS."
        return 1
    fi

    # 2. Look up our (module, kver, arch).
    if ! entry="$(_manifest_lookup "${manifest}" "${module}" "${kver}" "${karch}")"; then
        info "no prebuilt module for kernel ${kver} (${karch}); falling back to DKMS."
        return 1
    fi
    filename="$(printf '%s' "${entry}" | cut -f1)"
    sha256="$(printf '%s' "${entry}" | cut -f2)"
    vermagic="$(printf '%s' "${entry}" | cut -f3)"
    if [ -z "${filename}" ]; then
        warn "manifest entry missing filename; falling back to DKMS."
        return 1
    fi
    info "found prebuilt module ${filename} in manifest."

    # 3. Download the .ko + its sidecars.
    ko_url="${ADOS_PREBUILT_BASE_URL}/${filename}"
    ko_path="${tmp}/${filename}"
    if ! ados_fetch "${ko_url}" "${ko_path}"; then
        warn "could not download ${filename}; falling back to DKMS."
        return 1
    fi
    # Prefer the published sidecar; synthesize from the manifest hash when
    # the release did not ship a per-file .sha256 (keeps SHA256 mandatory).
    if ! ados_fetch "${ko_url}.sha256" "${ko_path}.sha256"; then
        if [ -n "${sha256}" ]; then
            printf '%s  %s\n' "${sha256}" "${filename}" > "${ko_path}.sha256"
        else
            warn "no sha256 for ${filename}; falling back to DKMS."
            return 1
        fi
    fi
    ados_fetch "${ko_url}.minisig" "${ko_path}.minisig" || true

    # 4. Verify SHA256 (+ signature when a real key is provisioned).
    if ! ados_verify_artifact "${ko_path}" "${pubkey}" "${channel}" \
            "${ADOS_PREBUILT_ALLOW_UNSIGNED:-0}"; then
        warn "prebuilt module failed verification; falling back to DKMS."
        return 1
    fi

    # 5. vermagic safety check against the running kernel.
    if ! _vermagic_ok "${ko_path}" "${vermagic}"; then
        warn "prebuilt module vermagic unsafe for running kernel; falling back to DKMS."
        return 1
    fi

    # 6. Install + load.
    dest="/lib/modules/${kver}/updates"
    if ! mkdir -p "${dest}"; then
        warn "could not create ${dest}; falling back to DKMS."
        return 1
    fi
    if ! install -m 0644 "${ko_path}" "${dest}/${module}.ko"; then
        warn "could not install module to ${dest}; falling back to DKMS."
        return 1
    fi
    if ! depmod -a "${kver}" >/dev/null 2>&1; then
        warn "depmod failed for ${kver}; falling back to DKMS."
        rm -f "${dest}/${module}.ko"
        return 1
    fi
    if ! modprobe "${module}" >/dev/null 2>&1; then
        warn "modprobe ${module} failed after prebuilt install; falling back to DKMS."
        rm -f "${dest}/${module}.ko"
        depmod -a "${kver}" >/dev/null 2>&1 || true
        return 1
    fi
    if ! lsmod | awk '{print $1}' | grep -qx "${module}"; then
        warn "${module} not loaded after modprobe; falling back to DKMS."
        rm -f "${dest}/${module}.ko"
        depmod -a "${kver}" >/dev/null 2>&1 || true
        return 1
    fi

    # 7. Breadcrumb so diagnostics and the GCS can report the module source.
    mkdir -p /run/ados 2>/dev/null || true
    printf 'prebuilt\n' > /run/ados/wfb-module-source 2>/dev/null || true

    info "loaded verified prebuilt ${module}.ko for kernel ${kver}."
    return 0
}
