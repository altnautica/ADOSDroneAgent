# Bootloader overlay activation for Allwinner and Rockchip boards.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# ----------------------------------------------------------------------------
# Allwinner activation: edit the bootloader config that exists
# ----------------------------------------------------------------------------
activate_overlay_sun55i() {
    local overlay_basename="$1"   # "cubie-a7z-waveshare35a"
    local marker="ados:${overlay_basename}"

    # extlinux.conf style (most generic U-Boot extlinux setups)
    if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ]; then
        if grep -q "${marker}" "${BOOT_DIR}/extlinux/extlinux.conf"; then
            info "extlinux.conf already references ${overlay_basename}; skipping."
            return 0
        fi
        # Snapshot before the edit so the boot probe can auto-revert if the
        # panel never binds. Brick-safety: a touched boot config is always
        # restorable.
        snapshot_boot_config "${BOOT_DIR}/extlinux/extlinux.conf" || return 1
        # Add an fdtoverlays line under each LABEL block. Anchor on a
        # known append line. If the structure is non-standard the
        # operator can fix it; this is a best-effort additive edit.
        if grep -q '^[[:space:]]*append' "${BOOT_DIR}/extlinux/extlinux.conf"; then
            sed -i "/^[[:space:]]*append/a\\
    fdtoverlays ${BOOT_DIR}/overlay-user/${overlay_basename}.dtbo  # ${marker}
" "${BOOT_DIR}/extlinux/extlinux.conf"
            info "Appended fdtoverlays line to extlinux.conf."
            return 0
        fi
        warn "extlinux.conf has no 'append' line; skipping automatic edit."
        return 1
    fi

    # Armbian / Orange Pi style env file
    if [ -f "${BOOT_DIR}/armbianEnv.txt" ] || [ -f "${BOOT_DIR}/orangepiEnv.txt" ]; then
        local env_file="${BOOT_DIR}/armbianEnv.txt"
        [ -f "${BOOT_DIR}/orangepiEnv.txt" ] && env_file="${BOOT_DIR}/orangepiEnv.txt"
        if grep -q "${marker}" "${env_file}"; then
            info "${env_file} already references ${overlay_basename}; skipping."
            return 0
        fi
        snapshot_boot_config "${env_file}" || return 1
        if grep -q '^user_overlays=' "${env_file}"; then
            sed -i -E "s/^user_overlays=(.*)\$/user_overlays=\1 ${overlay_basename}/" "${env_file}"
        else
            echo "user_overlays=${overlay_basename}  # ${marker}" >> "${env_file}"
        fi
        info "Updated user_overlays in ${env_file}."
        return 0
    fi

    warn "No supported boot-config file found (extlinux.conf / armbianEnv.txt / orangepiEnv.txt)."
    warn "Manual activation required: load DTBO from ${BOOT_DIR}/overlay-user/${overlay_basename}.dtbo."
    return 1
}

# ----------------------------------------------------------------------------
# Rockchip activation. Three supported styles in priority order:
#   1. Radxa OS Bookworm (rsdk-b1+): managed.list + u-boot-update
#   2. Older Radxa OS / Debian: /boot/dtb/rockchip/overlays-list
#   3. Armbian / Ubuntu-Rockchip: armbianEnv.txt + update-u-boot
# ----------------------------------------------------------------------------
activate_overlay_rk3588() {
    local overlay_name="$1"   # "rk3588-spi4-m2-cs0-waveshare35"
    local dtbo_src="${BOOT_DIR}/dtbo/${overlay_name}.dtbo"

    if [ ! -f "${dtbo_src}" ]; then
        warn "BSP DTBO not found at ${dtbo_src}; will compile from vendored source."
        return 1
    fi

    # Radxa OS Bookworm (rsdk-b1+): u-boot-update reads the dtbo dir
    # and regenerates extlinux.conf with fdtoverlays lines for every
    # .dtbo (without .disabled suffix). managed.list tracks which
    # entries the system "owns" so rsetup can rebuild cleanly across
    # kernel upgrades. The disabled state is the ".dtbo.disabled"
    # filename suffix; we keep the bare .dtbo we already installed.
    if [ -f "${BOOT_DIR}/dtbo/managed.list" ] && command -v u-boot-update >/dev/null 2>&1; then
        local entry="${overlay_name}.dtbo"
        local managed_changed=0
        if grep -qx "${entry}" "${BOOT_DIR}/dtbo/managed.list"; then
            info "managed.list already references ${entry}; skipping append."
        else
            echo "${entry}" >> "${BOOT_DIR}/dtbo/managed.list"
            info "Appended ${entry} to ${BOOT_DIR}/dtbo/managed.list"
            managed_changed=1
        fi
        # Defensive: rsetup convention is to keep INACTIVE overlays as
        # ".dtbo.disabled" and ACTIVE ones as bare ".dtbo". Our install
        # step already wrote the bare name so this is a no-op, but if
        # an older state lingered we drop the .disabled twin.
        local disabled_path="${BOOT_DIR}/dtbo/${overlay_name}.dtbo.disabled"
        if [ -f "${disabled_path}" ]; then
            rm -f "${disabled_path}"
            managed_changed=1
        fi

        # Idempotency guard: u-boot-update on Radxa Bookworm rewrites
        # extlinux.conf, which is the bootloader's only source of truth
        # on this board. A partial / interrupted rewrite can leave the
        # file truncated, which surfaces as a bricked board on the next
        # power cycle (no kernel load, total network silence). Only
        # invoke u-boot-update when managed.list actually changed AND the
        # existing extlinux.conf doesn't already list our overlay — both
        # must be true for the regen to be worth the boot risk.
        local already_referenced=0
        if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ] && \
            grep -q "${overlay_name}" "${BOOT_DIR}/extlinux/extlinux.conf"; then
            already_referenced=1
        fi
        if [ "${managed_changed}" -eq 0 ] && [ "${already_referenced}" -eq 1 ]; then
            info "extlinux.conf already references ${overlay_name}; skipping u-boot-update."
            return 0
        fi

        # Pre-snapshot extlinux.conf for restore-on-failure. If u-boot-update
        # corrupts it (truncates, writes invalid syntax, leaves it mid-write
        # on a crash), we restore the working file so the next power cycle
        # boots cleanly. The bench-bricking incident of 2026-05-20 was the
        # trigger for adding this guard.
        local extlinux="${BOOT_DIR}/extlinux/extlinux.conf"
        local extlinux_bak="${extlinux}.ados-bak"
        local size_before=0
        if [ -f "${extlinux}" ]; then
            size_before=$(wc -c < "${extlinux}" 2>/dev/null || echo 0)
            if cp -f "${extlinux}" "${extlinux_bak}" 2>/dev/null; then
                info "extlinux.conf snapshot saved to ${extlinux_bak} (${size_before} bytes)"
                # Record for the probation marker so the boot probe restores
                # this exact baseline, not a guessed path.
                BOOT_CONFIG_SNAPSHOT="${extlinux_bak}"
                BOOT_CONFIG_PATH="${extlinux}"
            else
                # Fail closed: never run u-boot-update (which rewrites the
                # bootloader's only config) without a restorable baseline.
                error "Could not snapshot ${extlinux}; refusing to run u-boot-update without a restorable baseline (brick-safety)."
                return 1
            fi
        fi

        info "Running u-boot-update to regenerate extlinux.conf..."
        if ! u-boot-update; then
            error "u-boot-update failed; restoring extlinux.conf from snapshot."
            if [ -f "${extlinux_bak}" ]; then
                cp -f "${extlinux_bak}" "${extlinux}"
                error "Restored ${extlinux} from ${extlinux_bak}."
            fi
            return 1
        fi

        # Size sanity check: a well-formed extlinux.conf on Radxa is
        # always > 200 bytes (label, kernel path, fdt path, append). If
        # we see a suspiciously small file after the regen, restore.
        local size_after=0
        if [ -f "${extlinux}" ]; then
            size_after=$(wc -c < "${extlinux}" 2>/dev/null || echo 0)
        fi
        if [ "${size_after}" -lt 100 ]; then
            error "extlinux.conf is suspiciously small (${size_after} bytes); restoring from snapshot."
            if [ -f "${extlinux_bak}" ]; then
                cp -f "${extlinux_bak}" "${extlinux}"
                error "Restored ${extlinux} from ${extlinux_bak} (was ${size_before} bytes)."
            fi
            return 1
        fi
        info "extlinux.conf regen OK (${size_before} -> ${size_after} bytes)."

        # Confirm the fdtoverlays line landed.
        if grep -q "${overlay_name}" "${BOOT_DIR}/extlinux/extlinux.conf"; then
            info "Overlay reference present in extlinux.conf."
        else
            warn "u-boot-update ran but extlinux.conf does not mention ${overlay_name} — check /etc/default/u-boot."
        fi
        return 0
    fi

    # Older Radxa OS / Debian Rockchip style.
    if [ -f "${BOOT_DIR}/dtb/rockchip/overlays-list" ]; then
        if grep -qx "${overlay_name}" "${BOOT_DIR}/dtb/rockchip/overlays-list"; then
            info "overlays-list already references ${overlay_name}; skipping."
        else
            echo "${overlay_name}" >> "${BOOT_DIR}/dtb/rockchip/overlays-list"
            info "Appended ${overlay_name} to overlays-list."
        fi
        return 0
    fi

    # Armbian Rockchip style.
    if command -v update-u-boot >/dev/null 2>&1; then
        if [ -f "${BOOT_DIR}/armbianEnv.txt" ]; then
            if grep -q "${overlay_name}" "${BOOT_DIR}/armbianEnv.txt"; then
                info "armbianEnv.txt already references ${overlay_name}; skipping."
            else
                # Snapshot before the edit so the boot probe can auto-revert
                # if the panel never binds. Fail closed: no blind edit without
                # a restorable baseline (brick-safety).
                snapshot_boot_config "${BOOT_DIR}/armbianEnv.txt" || return 1
                if grep -q '^user_overlays=' "${BOOT_DIR}/armbianEnv.txt"; then
                    sed -i -E "s/^user_overlays=(.*)\$/user_overlays=\1 ${overlay_name}/" "${BOOT_DIR}/armbianEnv.txt"
                else
                    echo "user_overlays=${overlay_name}" >> "${BOOT_DIR}/armbianEnv.txt"
                fi
                info "Updated user_overlays in armbianEnv.txt."
            fi
            update-u-boot >/dev/null 2>&1 || warn "update-u-boot returned non-zero; check log."
            return 0
        fi
    fi

    warn "Could not locate a supported Rockchip overlay activation file."
    return 1
}
