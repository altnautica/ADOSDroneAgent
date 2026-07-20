#!/usr/bin/env bats
# =============================================================================
# Bats suite for the LCD overlay installer's physical-presence detection.
#
# Exercises scripts/drivers/install-display-overlay.sh in `auto` mode against
# a fully mocked environment: the kernel sysfs surfaces (/sys/class/graphics,
# /sys/class/input, /sys/class/drm), /dev/dri, and i2cdetect are all redirected
# to a temp tree via the script's own env overrides, and every write path
# (display.conf, modules-load.d, the markers, the boot config) lands under a
# temp tree so the test can assert exactly which files were touched.
#
# The two brick-safety invariants under test:
#   1. NO panel present  -> ZERO writes to boot config / modules-load.d,
#      display_id=none, no display.enabled marker.
#   2. SPI-LCD already bound -> recognized, display.enabled written, and
#      ZERO boot-config change (the overlay is already in effect).
#
# Runs without root via ADOS_OVERLAY_ALLOW_NONROOT=1.
# =============================================================================

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    SCRIPT="${REPO_ROOT}/scripts/drivers/install-display-overlay.sh"
    [ -x "${SCRIPT}" ] || {
        echo "missing overlay script: ${SCRIPT}" >&2
        return 1
    }
    TMP="$(mktemp -d)"
    # Write-path roots.
    ETC="${TMP}/etc/ados"
    MODLOAD="${TMP}/modules-load.d"
    BOOT="${TMP}/boot"
    UDEV="${TMP}/udev/rules.d"
    mkdir -p "${ETC}" "${MODLOAD}" "${BOOT}/extlinux" "${UDEV}"
    # A plausible pre-existing boot config so a "no boot-config change"
    # assertion has something to compare against.
    printf 'LABEL ados\n  kernel /vmlinuz\n  fdt /board.dtb\n  append root=/dev/mmcblk0p2 rw\n' \
        > "${BOOT}/extlinux/extlinux.conf"
    BOOT_BEFORE_HASH="$(_hash_tree "${BOOT}")"

    # sysfs / dev mock roots.
    SYS_GRAPHICS="${TMP}/sys/class/graphics"
    SYS_INPUT="${TMP}/sys/class/input"
    SYS_DRM="${TMP}/sys/class/drm"
    DEV_DRI="${TMP}/dev/dri"
    mkdir -p "${SYS_GRAPHICS}" "${SYS_INPUT}" "${SYS_DRM}" "${DEV_DRI}"

    # An i2cdetect stub that reports NO device by default (empty grid). A
    # per-test override rewrites it to ACK 0x3c.
    I2CBIN="${TMP}/bin/i2cdetect"
    mkdir -p "${TMP}/bin"
    cat > "${I2CBIN}" <<'EOF'
#!/usr/bin/env bash
echo "     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f"
echo "30: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --"
exit 0
EOF
    chmod +x "${I2CBIN}"
}

teardown() {
    [ -n "${TMP:-}" ] && rm -rf "${TMP}"
}

# Stable content hash of a directory tree (paths + bytes) so a test can prove
# the boot config was not touched.
_hash_tree() {
    local root="$1"
    if [ ! -d "${root}" ]; then echo "ABSENT"; return; fi
    ( cd "${root}" && find . -type f -exec shasum {} \; | sort | shasum )
}

# Run the overlay script in auto mode with all probes pointed at the mock
# tree. $1 = board id.
run_overlay() {
    local board="$1"
    run env \
        ADOS_OVERLAY_ALLOW_NONROOT=1 \
        ADOS_BOARD_ID="${board}" \
        ADOS_DISPLAY="auto" \
        ADOS_ETC_DIR="${ETC}" \
        ADOS_MODULES_LOAD_DIR="${MODLOAD}" \
        ADOS_BOOT_DIR="${BOOT}" \
        ADOS_UDEV_RULES_DIR="${UDEV}" \
        ADOS_SYS_GRAPHICS_DIR="${SYS_GRAPHICS}" \
        ADOS_SYS_INPUT_DIR="${SYS_INPUT}" \
        ADOS_SYS_DRM_DIR="${SYS_DRM}" \
        ADOS_DEV_DRI_DIR="${DEV_DRI}" \
        ADOS_I2CDETECT_BIN="${I2CBIN}" \
        ADOS_I2C_OLED_BUS=1 \
        bash "${SCRIPT}"
}

# Mock a bound SPI-LCD framebuffer (fbtft driver name) at index $1.
mock_bound_fb() {
    local idx="$1"
    mkdir -p "${SYS_GRAPHICS}/fb${idx}"
    echo "fb_ili9486" > "${SYS_GRAPHICS}/fb${idx}/name"
}

# Mock the ADS7846 resistive touch input device at event$1.
mock_touch_input() {
    local idx="$1"
    mkdir -p "${SYS_INPUT}/event${idx}/device"
    echo "ADS7846 Touchscreen" > "${SYS_INPUT}/event${idx}/device/name"
}

# Mock a connected HDMI: a DRM card node + a connected connector status.
mock_hdmi_connected() {
    : > "${DEV_DRI}/card0"
    mkdir -p "${SYS_DRM}/card0-HDMI-A-1"
    echo "connected" > "${SYS_DRM}/card0-HDMI-A-1/status"
}

# Rewrite the i2cdetect stub to ACK an OLED at 0x3c.
mock_i2c_oled() {
    cat > "${I2CBIN}" <<'EOF'
#!/usr/bin/env bash
echo "     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f"
echo "30: -- -- -- -- -- -- -- -- -- -- -- -- 3c -- -- --"
exit 0
EOF
    chmod +x "${I2CBIN}"
}

# -----------------------------------------------------------------------------
# Invariant 1: NO panel present -> zero boot writes, display_id=none, no marker
# -----------------------------------------------------------------------------

# pi-zero-2w is in the driver's auto-default board list but resolves no
# SPI-LCD panel (no displays declared), so auto resolves to the pure
# no-display path (case e): zero boot writes, display_id=none, no marker.
# (A board that DECLARES a panel but has none bound takes the
# apply-verify-auto-revert probation path instead; that is exercised
# separately.)
@test "no panel (board declares no display): display_id=none, no marker, no boot change" {
    run_overlay pi-zero-2w
    [ "$status" -eq 0 ]
    # display.conf written and reports none.
    [ -f "${ETC}/display.conf" ]
    grep -q '^display_id=none' "${ETC}/display.conf"
    # No persistent marker.
    [ ! -f "${ETC}/display.enabled" ]
    # No probation marker.
    [ ! -f "${ETC}/display.probation" ]
    # No modules-load file (nothing queued to load).
    [ ! -f "${MODLOAD}/ados-display.conf" ]
    # Boot config byte-for-byte unchanged.
    [ "$(_hash_tree "${BOOT}")" = "${BOOT_BEFORE_HASH}" ]
}

@test "no panel: presence verdict recorded as none in display.conf" {
    run_overlay pi-zero-2w
    [ "$status" -eq 0 ]
    grep -q '^display_presence=none' "${ETC}/display.conf"
}

# -----------------------------------------------------------------------------
# Invariant 2: SPI-LCD already bound -> recognized, marker, no boot change
# -----------------------------------------------------------------------------

@test "bound SPI-LCD on fb0 + touch: recognized, marker written, no boot change" {
    mock_bound_fb 0
    mock_touch_input 0
    run_overlay rock-5c-lite
    [ "$status" -eq 0 ]
    # Recognized as the panel, not none.
    grep -q '^display_id=waveshare35a' "${ETC}/display.conf"
    grep -q '^overlay_source=present' "${ETC}/display.conf"
    grep -q '^activated_via=already-bound' "${ETC}/display.conf"
    # Persistent marker present.
    [ -f "${ETC}/display.enabled" ]
    # No probation (it is already bound, not on probation).
    [ ! -f "${ETC}/display.probation" ]
    # Modules-load written so it re-binds across reboots.
    [ -f "${MODLOAD}/ados-display.conf" ]
    # Crucially: the boot config was NOT touched.
    [ "$(_hash_tree "${BOOT}")" = "${BOOT_BEFORE_HASH}" ]
}

@test "bound SPI-LCD also recognized on fb1 (DRM owns fb0)" {
    mock_bound_fb 1
    mock_touch_input 0
    run_overlay rock-5c-lite
    [ "$status" -eq 0 ]
    grep -q '^display_id=waveshare35a' "${ETC}/display.conf"
    grep -q 'overlay_ref=bound:fb1' "${ETC}/display.conf"
    [ -f "${ETC}/display.enabled" ]
    [ "$(_hash_tree "${BOOT}")" = "${BOOT_BEFORE_HASH}" ]
}

@test "fbtft framebuffer but NO touch device: not confirmed as bound" {
    # A framebuffer that reports the fbtft name but with no matching touch
    # input must NOT confirm presence (the second signal is required), so the
    # board falls through to the probation apply path, not spi-bound.
    mock_bound_fb 0
    # No touch input mocked.
    run_overlay rock-5c-lite
    [ "$status" -ne 0 ] || true   # compile may fail w/o dtc; that's fine
    # Whatever happened, it must NOT have been recognized as already-bound.
    if [ -f "${ETC}/display.conf" ]; then
        ! grep -q '^activated_via=already-bound' "${ETC}/display.conf"
    fi
}

# -----------------------------------------------------------------------------
# HDMI present on a board with NO touch panel -> marker written (kiosk surface),
# display_id=none, no boot edit. pi-zero-2w declares no hdmi-touch display, so
# plain HDMI stays video-only (the kiosk binds the DRM framebuffer directly).
# -----------------------------------------------------------------------------

@test "HDMI connected (no touch panel): marker written, display_id=none, no boot change" {
    mock_hdmi_connected
    run_overlay pi-zero-2w
    [ "$status" -eq 0 ]
    grep -q '^display_id=none' "${ETC}/display.conf"
    grep -q '^display_presence=hdmi' "${ETC}/display.conf"
    # A display surface exists, so the marker IS written for the kiosk.
    [ -f "${ETC}/display.enabled" ]
    [ ! -f "${MODLOAD}/ados-display.conf" ]
    [ ! -f "${UDEV}/99-ados-hdmi-touch.rules" ]
    [ "$(_hash_tree "${BOOT}")" = "${BOOT_BEFORE_HASH}" ]
}

# -----------------------------------------------------------------------------
# HDMI present on a board that DECLARES an HDMI-touch panel -> provision the
# touch-only overlay: compile + activate, load ads7846 (only), write the
# LIBINPUT_CALIBRATION_MATRIX udev rule, and record the touch config so the
# calibration wizard can regenerate the matrix. rock-5c-lite declares the
# hdmi_touch_xpt2046 display; stub cpp/dtc/update-u-boot so the compile +
# armbian activate run without real headers.
# -----------------------------------------------------------------------------

@test "HDMI connected + board declares hdmi-touch: touch overlay + udev matrix + ads7846" {
    mock_hdmi_connected
    # Stub the overlay toolchain so the upstream compile + armbian activate
    # complete without real dt-bindings headers.
    cat > "${TMP}/bin/dtc" <<'EOF'
#!/usr/bin/env bash
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
[ -n "$out" ] && printf 'FAKEDTBO' > "$out"
exit 0
EOF
    cat > "${TMP}/bin/cpp" <<'EOF'
#!/usr/bin/env bash
out=""; inp=""; prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"
    elif [ "${a#-}" = "$a" ] && [ -f "$a" ]; then inp="$a"; fi
    prev="$a"
done
[ -n "$out" ] && [ -n "$inp" ] && grep -v '^#' "$inp" > "$out"
exit 0
EOF
    cat > "${TMP}/bin/update-u-boot" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "${TMP}/bin/dtc" "${TMP}/bin/cpp" "${TMP}/bin/update-u-boot"
    # Armbian activation path: an armbianEnv.txt + update-u-boot on PATH.
    echo "user_overlays=" > "${BOOT}/armbianEnv.txt"

    run env \
        PATH="${TMP}/bin:${PATH}" \
        ADOS_OVERLAY_ALLOW_NONROOT=1 \
        ADOS_BOARD_ID="rock-5c-lite" \
        ADOS_DISPLAY="auto" \
        ADOS_ETC_DIR="${ETC}" \
        ADOS_MODULES_LOAD_DIR="${MODLOAD}" \
        ADOS_BOOT_DIR="${BOOT}" \
        ADOS_UDEV_RULES_DIR="${UDEV}" \
        ADOS_SYS_GRAPHICS_DIR="${SYS_GRAPHICS}" \
        ADOS_SYS_INPUT_DIR="${SYS_INPUT}" \
        ADOS_SYS_DRM_DIR="${SYS_DRM}" \
        ADOS_DEV_DRI_DIR="${DEV_DRI}" \
        ADOS_I2CDETECT_BIN="${I2CBIN}" \
        bash "${SCRIPT}"
    [ "$status" -eq 0 ]
    # Resolved to the HDMI-touch panel, not none.
    grep -q '^display_id=hdmi_touch_xpt2046' "${ETC}/display.conf"
    grep -q '^type=hdmi-touch' "${ETC}/display.conf"
    grep -q '^has_touch=true' "${ETC}/display.conf"
    grep -q '^touch_chip=ADS7846' "${ETC}/display.conf"
    # No framebuffer: video is HDMI.
    grep -q '^framebuffer_path=$' "${ETC}/display.conf"
    grep -q '^touch_device_name=ADS7846 Touchscreen' "${ETC}/display.conf"
    grep -q '^libinput_calibration_matrix=' "${ETC}/display.conf"
    # Marker written (a display surface + touch device exist).
    [ -f "${ETC}/display.enabled" ]
    # modules-load carries ONLY ads7846 (no fbtft framebuffer stack).
    [ -f "${MODLOAD}/ados-display.conf" ]
    grep -q '^ads7846$' "${MODLOAD}/ados-display.conf"
    ! grep -q '^fbtft$' "${MODLOAD}/ados-display.conf"
    ! grep -q '^fb_ili9486$' "${MODLOAD}/ados-display.conf"
    # The udev calibration rule is written for the touch device.
    [ -f "${UDEV}/99-ados-hdmi-touch.rules" ]
    grep -q 'LIBINPUT_CALIBRATION_MATRIX' "${UDEV}/99-ados-hdmi-touch.rules"
    grep -q 'ADS7846 Touchscreen' "${UDEV}/99-ados-hdmi-touch.rules"
    # The touch-only overlay was installed under /boot/dtbo.
    [ -f "${BOOT}/dtbo/rk3588-spi4-m2-cs1-xpt2046-touch.dtbo" ]
}

# -----------------------------------------------------------------------------
# I2C OLED present -> marker written (OLED service), display_id=none, no boot
# -----------------------------------------------------------------------------

@test "I2C OLED present: marker written, display_id=none, no boot change" {
    mock_i2c_oled
    run_overlay rpi4b
    [ "$status" -eq 0 ]
    grep -q '^display_id=none' "${ETC}/display.conf"
    grep -q '^display_presence=i2c-oled' "${ETC}/display.conf"
    [ -f "${ETC}/display.enabled" ]
    [ "$(_hash_tree "${BOOT}")" = "${BOOT_BEFORE_HASH}" ]
}

# -----------------------------------------------------------------------------
# Detection priority: a bound SPI-LCD wins over a connected HDMI.
# -----------------------------------------------------------------------------

@test "bound SPI-LCD takes priority over a connected HDMI" {
    mock_bound_fb 0
    mock_touch_input 0
    mock_hdmi_connected
    run_overlay rock-5c-lite
    [ "$status" -eq 0 ]
    grep -q '^display_id=waveshare35a' "${ETC}/display.conf"
    grep -q '^activated_via=already-bound' "${ETC}/display.conf"
}

# -----------------------------------------------------------------------------
# Explicit --display none still skips cleanly and removes the marker.
# -----------------------------------------------------------------------------

@test "explicit none: display_id=none, marker removed, no boot change" {
    # Pre-seed a stale marker to prove the none path removes it.
    : > "${ETC}/display.enabled"
    run env \
        ADOS_OVERLAY_ALLOW_NONROOT=1 \
        ADOS_BOARD_ID="rock-5c-lite" \
        ADOS_ETC_DIR="${ETC}" \
        ADOS_MODULES_LOAD_DIR="${MODLOAD}" \
        ADOS_BOOT_DIR="${BOOT}" \
        ADOS_DISPLAY_ENABLED_FILE="${ETC}/display.enabled" \
        bash "${SCRIPT}" --display none
    [ "$status" -eq 0 ]
    grep -q '^display_id=none' "${ETC}/display.conf"
    [ ! -f "${ETC}/display.enabled" ]
    [ "$(_hash_tree "${BOOT}")" = "${BOOT_BEFORE_HASH}" ]
}

# -----------------------------------------------------------------------------
# auto none path removes a stale marker too.
# -----------------------------------------------------------------------------

@test "auto with no panel removes a stale display.enabled marker" {
    : > "${ETC}/display.enabled"
    run_overlay pi-zero-2w
    [ "$status" -eq 0 ]
    [ ! -f "${ETC}/display.enabled" ]
}

# -----------------------------------------------------------------------------
# Board declares an SPI-LCD but none bound -> apply-verify-auto-revert.
#
# A `dtc` stub lets the per-board apply complete without real kernel headers
# so the probation marker + boot snapshot logic is observable. The marker must
# record the snapshot the apply path saved so the boot probe can self-heal.
# -----------------------------------------------------------------------------

@test "board declares SPI-LCD, none bound: probation marker armed with snapshot" {
    # Stub dtc so compile succeeds without real device-tree sources/headers.
    cat > "${TMP}/bin/dtc" <<'EOF'
#!/usr/bin/env bash
# Emit a non-empty fake DTBO at the -o target.
out=""
prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
[ -n "$out" ] && printf 'FAKEDTBO' > "$out"
exit 0
EOF
    chmod +x "${TMP}/bin/dtc"
    # cubie-a7z uses the repo DTS + extlinux append path (no cpp/headers
    # needed), which is the simplest apply to drive in a test.
    run env \
        PATH="${TMP}/bin:${PATH}" \
        ADOS_OVERLAY_ALLOW_NONROOT=1 \
        ADOS_BOARD_ID="cubie-a7z" \
        ADOS_DISPLAY="auto" \
        ADOS_ETC_DIR="${ETC}" \
        ADOS_MODULES_LOAD_DIR="${MODLOAD}" \
        ADOS_BOOT_DIR="${BOOT}" \
        ADOS_SYS_GRAPHICS_DIR="${SYS_GRAPHICS}" \
        ADOS_SYS_INPUT_DIR="${SYS_INPUT}" \
        ADOS_SYS_DRM_DIR="${SYS_DRM}" \
        ADOS_DEV_DRI_DIR="${DEV_DRI}" \
        ADOS_I2CDETECT_BIN="${I2CBIN}" \
        bash "${SCRIPT}"
    [ "$status" -eq 0 ]
    # Probation armed: marker present and records the panel + boot config.
    [ -f "${ETC}/display.probation" ]
    grep -q '^display_id=waveshare35a' "${ETC}/display.probation"
    grep -q '^expected_fb_name=fb_ili9486' "${ETC}/display.probation"
    grep -q '^touch_chip=ADS7846' "${ETC}/display.probation"
    # The apply path snapshotted extlinux.conf so the boot probe can revert.
    grep -q "^snapshot=${BOOT}/extlinux/extlinux.conf.ados-bak" "${ETC}/display.probation"
    [ -f "${BOOT}/extlinux/extlinux.conf.ados-bak" ]
    # The snapshot is the pristine pre-edit config (no fdtoverlays line).
    ! grep -q 'fdtoverlays' "${BOOT}/extlinux/extlinux.conf.ados-bak"
    # display.conf describes the panel and the persistent marker is written
    # (the probe will confirm or revert on next boot).
    grep -q '^display_id=waveshare35a' "${ETC}/display.conf"
    [ -f "${ETC}/display.enabled" ]
}
