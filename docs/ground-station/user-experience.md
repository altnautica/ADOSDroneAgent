# ADOS Ground Station — User Experience

## First Boot Setup

1. Unbox ground station. Connect antenna to RP-SMA port.
2. Power on via USB-C (5V, any phone charger works for Lite).
3. Ground station boots in ~15 seconds. LED blinks green.
4. Auto-creates WiFi network `ADOS-GS-XXXX` (last 4 digits of device ID).
5. Connect phone or laptop to this WiFi.
6. Browser auto-opens captive portal with setup wizard.
7. Setup wizard: set a WiFi password, pair with air unit.
8. Pairing: scan QR code on air unit (or manually enter pairing key).
9. Done. Ground station saves config and reboots into operational mode.

Total setup time: under 2 minutes.

## Normal Operation (Field)

1. Power on drone (with air unit) and ground station.
2. WFB-ng link auto-establishes in ~3 seconds (pre-paired keys).
3. Ground station LED turns solid green (link active).
4. Connect phone/laptop to `ADOS-GS-XXXX` WiFi.
5. Open any browser. Navigate to `192.168.4.1:8080`.
6. Live video + telemetry + map appear in ADOS Mission Control.
7. Full GCS capability: arm/disarm, mode change, mission planning, video recording.

No app install. No driver install. No Linux terminal. Just WiFi and a browser.

## Pairing Mechanism

Air unit and ground station share WFB-ng encryption keys.

| Step | Action | Detail |
|------|--------|--------|
| 1 | Key generation | `wfb_keygen` generates a keypair during factory provisioning |
| 2 | Key distribution | QR code on air unit label encodes the pairing key |
| 3 | Pairing | Ground station setup wizard scans QR or accepts manual key entry |
| 4 | Key storage | Keys stored at `/etc/ados/wfb.key` on both devices |
| 5 | Verification | Ground station attempts WFB-ng handshake to confirm match |

Keys persist across reboots. Re-pairing only needed when replacing hardware.

## Multiple Viewers

The ground station WiFi AP supports 5-10 simultaneous browser clients. Each viewer sees the same video feed (mediamtx multicast). No additional configuration needed.

| Scenario | Viewers | Example |
|----------|---------|---------|
| Solo operator | 1 | Pilot with phone |
| Pilot + observer | 2 | Pilot on tablet, observer on laptop |
| Training | 3+ | Instructor + student + safety officer |
| Demo/presentation | 5-10 | Live flight demo for investors or partners |

## Factory Reset

Hold user button for 10+ seconds. The ground station:
- Wipes all config (WiFi password, pairing keys, custom settings)
- Reboots into setup wizard mode
- LED blinks fast yellow during reset

After reset, repeat the first boot setup flow.

## Display Modes

| Mode | Variant | How It Works | Latency | Best For |
|------|---------|-------------|---------|----------|
| Browser-only | Lite | All video via WebRTC in browser | 50-80ms | Portable, phone-based |
| HDMI output | Pro | Direct video on attached monitor | 35-55ms | Fixed ground station |
| FPV goggles | Pro | HDMI output to goggles | 35-55ms | Immersive pilot view |
| Dual display | Pro | HDMI for pilot video, browser for map + telemetry | Mixed | Professional operations |

The dual display mode is the most capable setup: the pilot watches low-latency HDMI video on goggles or a monitor, while a second person manages the mission on a laptop via the browser GCS.

## LED Status Indicators

| LED Pattern | Meaning |
|------------|---------|
| Off | No power |
| Solid red | Booting |
| Blinking green | Ready, waiting for WFB-ng link |
| Solid green | WFB-ng link active, video streaming |
| Blinking yellow | Setup wizard mode (first boot or factory reset) |
| Solid yellow | Error (check REST API status endpoint) |
| Blinking red | Hardware fault (adapter not detected) |

## Field-Only Tap-to-Pair (Relay and Receiver Roles)

When two or three ground nodes work together in the distributed-receive setup, they join a batman-adv mesh and exchange a shared pairing credential so only approved nodes carry traffic. The flow runs entirely from the OLED on each node, no laptop needed.

1. On the node you want to make the hub, open the menu and navigate to `Mesh > Set role`. Pick `Receiver`. The service restart completes in a few seconds.
2. Still on the receiver, navigate to `Mesh > Accept relay` and press Select. The screen shows a countdown. During this window, the receiver listens for pairing requests on its mesh interface. Any relays that send a request appear inline on the same screen, with approve and reject actions.
3. On each relay node, open the menu and navigate to `Mesh > Set role`. Pick `Relay`. After the restart, navigate to `Mesh > Join mesh` and pick the receiver from the scan list. The relay sends its signed invite request to the receiver and waits.
4. Back on the receiver, pending relays are listed on the `Accept relay` screen. Scroll to each one and approve.
5. The relay OLED status line now shows the mesh as linked. Video fragments start flowing.

If the accept window expires before every relay has joined, reopen another window on the receiver. Rejected or revoked relays can be re-added the same way.

**No QR code. No phone app. No laptop.** The operator only touches the OLED and the 4 buttons. The invite bundle is a short signed message that travels over UDP on the mesh interface, so there is no cable or IP configuration to worry about.

**Factory reset wipes pairing state.** A long hold on the Back button during boot (or `sudo ados gs reset --confirm <pair-key-fingerprint>`; use `factory-reset-unpaired` when the node has not been paired yet) removes the mesh identity, the pairing invite bundle, and the approved-relay list. The node returns to `direct` role.

## REST API Status

`GET http://192.168.4.1:8080/api/status` returns JSON with service health:

```json
{
  "mode": "rx",
  "wfb": { "status": "connected", "rssi": -45, "snr": 28 },
  "video": { "status": "streaming", "resolution": "1920x1080", "fps": 30 },
  "mavlink": { "status": "active", "heartbeat_hz": 1.0 },
  "wifi_ap": { "clients": 2, "ssid": "ADOS-GS-A1B2" },
  "uptime_seconds": 3842
}
```

Useful for debugging, monitoring, and integration with third-party tools.
