# OEM Deployment Guide — Manufacturing Integration

This document covers the full process of flashing, testing, and shipping ADOS ADOS Drone Agent on companion computer hardware.

---

## 1. Pre-Production Checklist

Before starting production, verify all items:

### Hardware
- [ ] Baseboard PCB finalized and tested (rev confirmed)
- [ ] Compute module sourced (Radxa CM3 or CM4, or Luckfox Pico variants)
- [ ] UART connection to flight controller verified (TX/RX/GND, 3.3V logic)
- [ ] USB camera tested (UVC compatible, 720p minimum)
- [ ] 4G modem tested (Quectel EC200 series recommended, USB or UART)
- [ ] SIM card slot accessible without disassembly
- [ ] WiFi antenna placement verified (no metal shielding between antenna and case exterior)
- [ ] User button wired (GPIO, active low, for factory reset)
- [ ] Status LED wired (GPIO, active high)
- [ ] Power input tested (5V 2A minimum, 5V 3A recommended for camera + modem)

### Software
- [ ] Production Linux image built (Buildroot or Debian, board-specific)
- [ ] Board YAML profile created and tested (`boards/your-board-name.yaml`)
- [ ] Agent starts on boot via systemd
- [ ] WiFi AP mode works on first boot
- [ ] Config webapp loads in browser
- [ ] MAVLink proxy connects to FC over UART
- [ ] Video streaming works (camera to WebRTC)
- [ ] 4G modem connects and gets IP

### Cloud
- [ ] MQTT broker deployed (self-hosted or Altnautica-hosted)
- [ ] STUN/TURN servers configured
- [ ] Device provisioning process documented for factory line

### Manufacturing
- [ ] Flash station set up (USB hub, power supply, host PC)
- [ ] QC test script running on test station
- [ ] Packaging materials ready (enclosure, antenna, cables, quick start card)

### Documentation
- [ ] Quick start card finalized (single sheet, both sides)
- [ ] User manual available (PDF or online)
- [ ] Support contact and warranty terms printed

---

## 2. Flash Process

Three methods depending on your production scale and compute module.

### Method A: Rockchip FactoryTool (batch USB flashing)

Best for: Production runs of 50+ units with Rockchip-based modules (RV1103, RV1106, RK3506, RK3566, RK3588S2).

FactoryTool supports up to 24 devices simultaneously over USB.

**Setup:**
1. Install Rockchip FactoryTool on Windows host PC
2. Load the production `.img` file
3. Connect USB hub (powered, 24-port recommended)
4. Put each module into maskrom mode:
   - Hold BOOT button while connecting USB, OR
   - Short maskrom pads on baseboard (board-specific)
5. FactoryTool auto-detects devices and begins flashing
6. Green = success, Red = failed (retry or reject unit)

**Typical flash time:** 2-5 minutes per unit depending on image size and USB speed.

### Method B: rkdeveloptool (single unit, development)

Best for: Engineering samples, debugging, individual units.

```bash
# Install
sudo apt install rkdeveloptool

# Put device in maskrom mode, then:
rkdeveloptool db loader.bin
rkdeveloptool wl 0 production-image.img
rkdeveloptool rd   # reboot
```

### Method C: SD Card (dd or balenaEtcher)

Best for: Modules that boot from SD card (Luckfox Pico, some Radxa configs).

```bash
# Linux/Mac
sudo dd if=production-image.img of=/dev/sdX bs=4M status=progress
sync

# Or use balenaEtcher (GUI, cross-platform)
# Select image → Select SD card → Flash
```

For production: pre-flash SD cards in bulk using a multi-slot SD duplicator.

---

## 3. QC Test Script

Every unit must pass the QC test before shipping. The test script runs automatically when the device boots with a test jumper installed (or via SSH from the test station).

### Running the Test

```bash
# SSH into the device (default credentials during QC)
ssh root@192.168.4.1

# Run QC test
ados qc-test --full
```

### What It Tests

| Test | Method | Pass Criteria |
|------|--------|---------------|
| UART | Send MAVLink heartbeat, check response | FC responds within 2 seconds |
| USB | Enumerate USB devices | Camera and modem detected |
| Camera | Capture single frame via v4l2 | Frame received, resolution correct |
| WiFi | Scan for networks in STA mode | At least 1 network found |
| WiFi AP | Start AP, check interface up | AP SSID broadcasts |
| LED | Blink pattern (3x fast) | Visual confirmation (or photodiode) |
| 4G Modem | AT command check | Modem responds to `AT+CPIN?` |
| SIM | Check SIM status | `+CPIN: READY` (if SIM inserted) |
| GPIO | Toggle user button GPIO | Button read matches expected state |
| Storage | Write/read test file | File integrity verified |
| Memory | Check available RAM | Above minimum threshold (128MB for RV1106, 1GB for CM3) |

### Output

```
[PASS] UART: FC heartbeat received (ArduPilot 4.5.7)
[PASS] USB: 3 devices (camera, modem, hub)
[PASS] Camera: 1280x720 frame captured
[PASS] WiFi: 4 networks found
[PASS] WiFi AP: ADOS-A3F2 broadcasting
[PASS] LED: blink sequence complete
[PASS] Modem: Quectel EC200A detected
[SKIP] SIM: no SIM inserted
[PASS] GPIO: button reads correct
[PASS] Storage: 12.4GB available
[PASS] Memory: 468MB available

RESULT: 10/11 PASS, 0 FAIL, 1 SKIP
VERDICT: SHIP
```

Failed units get a detailed log at `/var/log/ados-qc.log` for diagnosis.

---

## 4. First-Boot Flow

What happens when a customer powers up the device for the first time.

```
Power on
  → Linux boots (3-8 seconds depending on chip)
  → Boot splash displayed (OEM-branded)
  → systemd starts ados agent
  → Agent detects: no config file at /etc/ados/config.yaml
  → Agent enters SETUP mode:
      1. Enables WiFi AP: "ADOS-XXXX" (last 4 of MAC)
      2. Starts captive portal on 192.168.4.1:80
      3. LED blinks slow (1Hz) = setup mode
  → User connects phone/laptop to WiFi
  → Browser opens setup wizard automatically (captive portal redirect)
  → Setup wizard steps:
      Step 1: Welcome + language selection
      Step 2: WiFi credentials (connect to home/field network)
      Step 3: Flight controller connection (auto-detect baud rate)
      Step 4: Video source (camera selection if multiple)
      Step 5: Cloud connection (MQTT broker URL, optional)
      Step 6: Summary + confirm
  → User clicks "Save and Reboot"
  → Config written to /etc/ados/config.yaml
  → Device reboots into OPERATIONAL mode
  → LED solid green = connected and running
  → WiFi AP disabled (device joins configured network)
```

### Setup Mode Timeout

If no one connects to the AP within 30 minutes, the device reboots and tries again. This prevents the AP from running indefinitely and draining power.

---

## 5. Factory Reset

**Trigger:** Hold the user button for more than 10 seconds. LED flashes fast (5Hz) to confirm.

**What it does:**
1. Deletes `/etc/ados/config.yaml`
2. Deletes `/etc/ados/device-id` (new identity on next boot)
3. Deletes TLS certificates from `/etc/ados/certs/`
4. Clears WiFi saved networks
5. Clears MQTT credentials
6. Reboots into setup mode (WiFi AP + captive portal)

**What it does NOT do:**
- Does not reflash the OS or agent software
- Does not change the firmware version
- Does not wipe flight logs (stored on SD card partition if available)

For a full reflash (OS + agent), use the flash process from Section 2.

---

## 6. Packaging Considerations

### Quick Start Card

Single sheet, credit-card-to-postcard size. Both sides. Include:

**Front:**
- Product name and OEM logo
- "Getting Started" in 4 steps with icons:
  1. Connect to flight controller (UART cable diagram)
  2. Power on (USB-C or JST connector)
  3. Connect phone to "ADOS-XXXX" WiFi
  4. Open browser, follow setup wizard

**Back:**
- LED status codes (solid green = running, blinking = setup, fast blink = error, red = fault)
- Factory reset instructions (hold button 10 seconds)
- Support URL and QR code
- Regulatory marks (FCC, CE as applicable)

### Antenna Placement

- WiFi antenna: External SMA or U.FL pigtail. Must not be blocked by metal enclosure.
- 4G antenna: External SMA recommended. Two antennas for MIMO (main + diversity).
- Keep WiFi and 4G antennas at least 5cm apart to avoid interference.

### SIM Slot Access

- SIM tray or push-push slot must be accessible without opening the enclosure.
- Label the SIM slot clearly (nano-SIM or micro-SIM).
- Include a note: "Insert SIM before powering on."

### Cable Routing

- UART cable to FC: JST-GH 1.25mm (matches most FCs) or bare wires with labels (TX, RX, GND).
- USB-C for power and/or data.
- Include the UART cable in the box. Customers will lose patience if they need to source one.
