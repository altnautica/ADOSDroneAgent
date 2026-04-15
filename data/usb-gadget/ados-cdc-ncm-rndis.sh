#!/bin/bash
# ADOS Drone Agent libcomposite USB gadget (CDC-NCM + RNDIS).
#
# MSN-029 Cellos Wave 1 H2. Runs as a systemd oneshot on the
# ground-station profile when ADOS_ENABLE_USB_GADGET=1 was set at
# install time. Creates a composite gadget so macOS/Linux hosts bind
# CDC-NCM and Windows hosts bind RNDIS, both presenting the ground
# station as a USB-C network adapter.
#
# Idempotent: if the gadget directory already exists and a UDC is
# bound, this script exits 0 without touching anything.
#
# Vendor/Product: idVendor=0x1d6b (Linux Foundation),
# idProduct=0x0104 (Multifunction Composite Gadget).

set -eu

GADGET_NAME="ados0"
GADGET_DIR="/sys/kernel/config/usb_gadget/${GADGET_NAME}"
CONFIGFS_ROOT="/sys/kernel/config"

VID="0x1d6b"
PID="0x0104"
BCDDEVICE="0x0100"
BCDUSB="0x0200"
MANUFACTURER="Altnautica"
PRODUCT="ADOS Ground Station"

log() { echo "[ados-usb-gadget] $*"; }

# Ensure configfs is mounted. Radxa/Debian images auto-mount it via
# fstab but buildroot and minimal images sometimes do not.
if [ ! -d "${CONFIGFS_ROOT}/usb_gadget" ]; then
    if ! mount | grep -q "on ${CONFIGFS_ROOT} "; then
        log "Mounting configfs at ${CONFIGFS_ROOT}..."
        mount -t configfs none "${CONFIGFS_ROOT}" || {
            log "ERROR: configfs mount failed; kernel likely built without CONFIGFS_FS."
            exit 1
        }
    fi
    # Load libcomposite so usb_gadget subdir appears.
    modprobe libcomposite 2>/dev/null || true
    if [ ! -d "${CONFIGFS_ROOT}/usb_gadget" ]; then
        log "ERROR: ${CONFIGFS_ROOT}/usb_gadget missing after modprobe libcomposite."
        exit 1
    fi
fi

# Idempotency guard: if the gadget tree is already built AND bound to
# a UDC, exit clean.
if [ -d "${GADGET_DIR}" ]; then
    current_udc="$(cat "${GADGET_DIR}/UDC" 2>/dev/null || true)"
    if [ -n "${current_udc}" ]; then
        log "Gadget ${GADGET_NAME} already bound to UDC ${current_udc}; nothing to do."
        exit 0
    fi
    log "Gadget ${GADGET_NAME} exists but not bound; attempting re-bind."
else
    log "Creating gadget ${GADGET_NAME}..."
    mkdir -p "${GADGET_DIR}"

    echo "${VID}"         > "${GADGET_DIR}/idVendor"
    echo "${PID}"         > "${GADGET_DIR}/idProduct"
    echo "${BCDDEVICE}"   > "${GADGET_DIR}/bcdDevice"
    echo "${BCDUSB}"      > "${GADGET_DIR}/bcdUSB"

    # Device class 0xEF/0x02/0x01 = Interface Association Descriptor
    # (IAD). Required for composite CDC-NCM + RNDIS on Windows.
    echo "0xEF" > "${GADGET_DIR}/bDeviceClass"
    echo "0x02" > "${GADGET_DIR}/bDeviceSubClass"
    echo "0x01" > "${GADGET_DIR}/bDeviceProtocol"

    # Derive a stable serial from the device-id file written by the
    # agent at first boot. Fall back to machine-id.
    serial="unknown"
    if [ -f /var/lib/ados/device-id ]; then
        serial="$(cat /var/lib/ados/device-id | tr -d '[:space:]' | head -c 32)"
    elif [ -f /etc/machine-id ]; then
        serial="$(cat /etc/machine-id | tr -d '[:space:]' | head -c 32)"
    fi

    mkdir -p "${GADGET_DIR}/strings/0x409"
    echo "${serial}"        > "${GADGET_DIR}/strings/0x409/serialnumber"
    echo "${MANUFACTURER}"  > "${GADGET_DIR}/strings/0x409/manufacturer"
    echo "${PRODUCT}"       > "${GADGET_DIR}/strings/0x409/product"

    # Derive deterministic MAC addresses from the serial so repeat
    # boots present the same adapter to the host.
    mac_suffix="$(printf '%s' "${serial}" | md5sum | cut -c1-10)"
    host_mac="02:$(echo "${mac_suffix}" | sed 's/\(..\)/\1:/g; s/:$//')"
    dev_mac="06:$(echo "${mac_suffix}"  | sed 's/\(..\)/\1:/g; s/:$//')"

    # Configuration 1: RNDIS first (Windows auto-binds to the first
    # config it understands, CDC-NCM fallback goes after).
    mkdir -p "${GADGET_DIR}/configs/c.1/strings/0x409"
    echo "ADOS Composite" > "${GADGET_DIR}/configs/c.1/strings/0x409/configuration"
    echo "250"            > "${GADGET_DIR}/configs/c.1/MaxPower"

    # RNDIS function (Windows).
    mkdir -p "${GADGET_DIR}/functions/rndis.usb0"
    echo "${host_mac}" > "${GADGET_DIR}/functions/rndis.usb0/host_addr"
    echo "${dev_mac}"  > "${GADGET_DIR}/functions/rndis.usb0/dev_addr"
    # Microsoft OS descriptors so Windows auto-loads RNDIS driver.
    if [ -d "${GADGET_DIR}/os_desc" ]; then
        echo "1"       > "${GADGET_DIR}/os_desc/use"
        echo "0xcd"    > "${GADGET_DIR}/os_desc/b_vendor_code"
        echo "MSFT100" > "${GADGET_DIR}/os_desc/qw_sign"
    fi
    if [ -d "${GADGET_DIR}/functions/rndis.usb0/os_desc/interface.rndis" ]; then
        echo "RNDIS"       > "${GADGET_DIR}/functions/rndis.usb0/os_desc/interface.rndis/compatible_id"
        echo "5162001"     > "${GADGET_DIR}/functions/rndis.usb0/os_desc/interface.rndis/sub_compatible_id"
    fi

    # CDC-NCM function (macOS, Linux, Android).
    mkdir -p "${GADGET_DIR}/functions/ncm.usb0"
    echo "${host_mac}" > "${GADGET_DIR}/functions/ncm.usb0/host_addr"
    echo "${dev_mac}"  > "${GADGET_DIR}/functions/ncm.usb0/dev_addr"

    ln -sf "${GADGET_DIR}/functions/rndis.usb0" "${GADGET_DIR}/configs/c.1/rndis.usb0"
    ln -sf "${GADGET_DIR}/functions/ncm.usb0"   "${GADGET_DIR}/configs/c.1/ncm.usb0"
    if [ -d "${GADGET_DIR}/os_desc" ]; then
        ln -sf "${GADGET_DIR}/configs/c.1" "${GADGET_DIR}/os_desc/c.1" 2>/dev/null || true
    fi
fi

# Bind to the first available UDC. `ls /sys/class/udc` returns one
# name per detected USB device controller.
udc=""
if [ -d /sys/class/udc ]; then
    udc="$(ls /sys/class/udc 2>/dev/null | head -n1 || true)"
fi
if [ -z "${udc}" ]; then
    log "ERROR: no UDC found under /sys/class/udc; is dwc2 overlay + module loaded?"
    exit 1
fi
log "Binding gadget to UDC ${udc}..."
echo "${udc}" > "${GADGET_DIR}/UDC"
log "Gadget ${GADGET_NAME} online."
exit 0
