# ADOS Ground Station — Software Architecture

## Overview

Single Python process (asyncio), same `ados` package as the air unit. The mode flag (`wfb.mode: rx`) switches all services from transmit to receive behavior. One codebase, two products.

## Service Layout

```
┌────────────────────────────────────────────────┐
│         ADOS Ground Station (single process)    │
│                                                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐      │
│  │ wfb_rx   │  │ mediamtx │  │ MAVLink  │      │
│  │ (receive │  │ (WebRTC  │  │ Relay    │      │
│  │  video + │  │  relay   │  │ (WS/TCP/ │      │
│  │  telem)  │  │  to      │  │  UDP to  │      │
│  │          │  │  browser) │  │  GCS)    │      │
│  └──────────┘  └──────────┘  └──────────┘      │
│  ┌──────────┐  ┌──────────┐                     │
│  │ WiFi AP  │  │ REST API │                     │
│  │ +Captive │  │ + WebApp │                     │
│  │  Portal  │  │ (:8080)  │                     │
│  └──────────┘  └──────────┘                     │
└────────────────────────────────────────────────┘
```

## Data Flow

```
DRONE (air unit):
  Camera → H.264 HW encode → wfb_tx → RTL8812EU (5.8GHz broadcast)
      ↓ (RF, 30-70ms, 5-50km)
GROUND STATION:
  RTL8812EU (RX) → wfb_rx → H.264 stream
      ↓
  mediamtx (RTSP input → WebRTC output)
      ↓
  WiFi AP (ADOS-GS-XXXX)
      ↓
  Phone/Laptop browser → WebRTC video + MAVLink telemetry
```

## Configuration Differences

| Setting | Air Unit | Ground Station |
|---------|----------|---------------|
| `wfb.mode` | `tx` | `rx` |
| `video.enabled` | `true` (captures from camera) | `false` (receives, does not capture) |
| `mavlink.serial_port` | `/dev/ttyAMA0` (UART to FC) | `none` |
| `wifi_ap.enabled` | `false` (optional) | `true` (always) |
| `mediamtx.enabled` | `false` | `true` |

## Boot Sequence

1. Detect mode from config file (`/etc/ados/config.yaml`)
2. Start `wfb_rx` (monitor mode, listen on configured channel)
3. Start `mediamtx` (RTSP ingest from wfb_rx, WebRTC output)
4. Start WiFi AP (`hostapd`, SSID: `ADOS-GS-XXXX`)
5. Start REST API + web app on `:8080`
6. Start MAVLink relay (forward telemetry from wfb_rx to WebSocket)
7. LED solid green when all services healthy

## Memory Estimate (Ground Station on RK3566)

| Service | RAM Usage |
|---------|----------|
| wfb_rx | ~20 MB |
| mediamtx | ~50 MB |
| Python (ados) | ~30 MB |
| hostapd + dnsmasq | ~5 MB |
| OS (Armbian minimal) | ~80 MB |
| **Total** | **~185 MB** |

Fits comfortably in 2GB RAM (Radxa CM3 Lite variant).

## Key Design Decisions

- **mediamtx for video relay.** Accepts RTSP from wfb_rx, serves WebRTC to browsers. No transcoding. Copy codec only. Adds ~1-3ms latency.
- **WiFi AP instead of Ethernet.** Most field operators carry phones, not laptops with Ethernet cables. WiFi AP gives instant connectivity without cables.
- **Captive portal for first boot.** Auto-redirects to setup wizard when user connects to WiFi. No need to know the IP address.
- **WebRTC over HLS/DASH.** WebRTC gives sub-second latency. HLS/DASH add 3-10 seconds of buffering. Not acceptable for drone video.
- **MAVLink over WebSocket.** Browsers cannot open raw TCP/UDP sockets. WebSocket is the only option for bidirectional binary data in a browser.
