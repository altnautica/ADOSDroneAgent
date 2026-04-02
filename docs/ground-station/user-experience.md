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
