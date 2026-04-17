# Platform Compatibility — OS Support Matrix

WFB-ng requires WiFi adapters in monitor mode with packet injection. This is a Linux-only capability. macOS, Windows, Android, and iOS do not expose the necessary low-level WiFi APIs. The ADOS ground station exists because of this gap.

## The Problem

WFB-ng (WiFi Broadcast next generation) works by putting a WiFi adapter into monitor mode and injecting raw 802.11 frames. This bypasses the normal WiFi stack entirely. No association, no authentication, no TCP/IP overhead. Just raw packets over the air, with FEC (forward error correction) to handle packet loss. This is what gives WFB-ng its low latency and long range.

But monitor mode with packet injection requires kernel-level driver support that exposes raw frame transmission and reception. Only Linux provides this through its mac80211 subsystem and patched RTL8812AU/EU drivers.

This is not a temporary software limitation. Apple, Microsoft, and Google deliberately restrict raw WiFi access for security reasons. There is no workaround, no hack, no special driver that enables it on these platforms. The only path to WFB-ng performance is a hardware device running Linux.

## Direct WFB-ng Support by Platform

| Platform | Monitor Mode | Packet Injection | WFB-ng Native | Why |
|----------|-------------|-------------------|---------------|-----|
| Linux (Debian/Ubuntu/Armbian) | Yes | Yes | Yes | Full kernel driver support via mac80211. Patched rtl8812eu driver enables monitor mode and raw frame injection |
| macOS | No | No | No | Apple's CoreWLAN framework does not expose monitor mode to userspace. The WiFi chipset supports it at the hardware level, but macOS blocks all access. No known workaround exists |
| Windows | No | No | No | Windows NDIS framework does not support packet injection on modern WiFi drivers. Historical options like AirPcap required dedicated capture hardware. No RTL8812 injection driver exists for Windows |
| Android | No | No | No | Android's WiFi HAL does not expose monitor mode to applications. Root access with a custom kernel can theoretically enable it, but no stable implementation exists for external USB WiFi adapters |
| iOS / iPadOS | No | No | No | Apple restricts all low-level WiFi access. No monitor mode, no raw frame injection, no support for external WiFi adapters via Lightning or USB-C |

## Via ADOS Ground Station (Browser Access)

The ADOS ground station runs WFB-ng on a Linux SBC (handling the hard part), then creates a WiFi access point and serves video and telemetry to any connected device via WebRTC in a browser (the easy part).

| Platform | Connect to GS WiFi | Open Browser | Watch Video | Send Commands | Full GCS | Works? |
|----------|-------------------|-------------|-------------|---------------|----------|--------|
| Linux | Yes | Yes | Yes (WebRTC) | Yes (MAVLink) | Yes | Yes |
| macOS | Yes | Yes | Yes (WebRTC) | Yes (MAVLink) | Yes | Yes |
| Windows | Yes | Yes | Yes (WebRTC) | Yes (MAVLink) | Yes | Yes |
| Android | Yes | Yes | Yes (WebRTC) | Yes (MAVLink) | Yes | Yes |
| iOS / iPadOS | Yes | Yes | Yes (WebRTC) | Yes (MAVLink) | Yes | Yes |
| ChromeOS | Yes | Yes | Yes (WebRTC) | Yes (MAVLink) | Yes | Yes |

Every platform that can connect to WiFi and open a web browser gets full access to video, telemetry, drone control, and the ADOS Mission Control GCS. The ground station handles the Linux-specific WFB-ng work internally.

## Why a Dedicated Ground Node

WFB-ng requires a Linux host with monitor-mode drivers. Mac, Windows, Android, and iOS do not expose the necessary low-level WiFi APIs. A dedicated Linux ground node handles WFB-ng internally and exposes video and telemetry to any connected device over standard web protocols (WiFi AP + WebRTC + WebSocket), so the client device does not need Linux or a compatible adapter.

## Mesh Capability by Host Board

A node can act as `relay` or `receiver` if it has a second USB port (beyond the one used by the RTL8812EU) AND a kernel with the `batman-adv` module. Both are standard on every supported board once the `--with-mesh` install option runs apt install for `batctl` and related packages.

| Board | Second USB port for mesh dongle | Kernel batman-adv module | Mesh capable |
|-------|---|---|---|
| Raspberry Pi 4B | Yes (4× USB total) | Yes (in-tree) | Yes |
| Raspberry Pi 5 | Yes (4× USB total) | Yes (in-tree) | Yes |
| Radxa Rock 5C / 5C Lite | Yes (2× USB 2.0 + 2× USB 3.0) | Yes (in-tree) | Yes |
| Radxa CM3 + carrier | Depends on carrier | Yes (in-tree) | Yes if carrier exposes ≥ 2 USB ports |
| Radxa CM4 + carrier | Depends on carrier | Yes (in-tree) | Yes if carrier exposes ≥ 2 USB ports |
| USB Dongle variant | No (host-dependent) | Host-dependent | Not applicable (not a standalone node) |

`direct` role works on every board above without the mesh dongle. Mesh support is an additional capability, not a prerequisite.

## Browser Requirements

Any modern browser with WebRTC support works. No extensions, plugins, or installations required.

| Browser | Minimum Version | WebRTC | Notes |
|---------|----------------|--------|-------|
| Chrome | 80+ | Yes | Recommended. Best WebRTC performance and compatibility |
| Firefox | 78+ | Yes | Full support. Good alternative to Chrome |
| Safari | 14.1+ | Yes | Requires macOS 11 (Big Sur) or iOS 14.5+. Works on iPhone and iPad |
| Edge | 80+ | Yes | Chromium-based, same engine as Chrome |
| Samsung Internet | 12+ | Yes | Common default on Samsung Android devices |
| Opera | 67+ | Yes | Chromium-based |

Browsers released before 2020 may lack WebRTC support. Update to any recent version.

### What Runs in the Browser

- **Video:** WebRTC stream from the ground station's mediamtx instance. Hardware-decoded by the browser. Sub-100ms latency.
- **Telemetry:** WebSocket connection carrying MAVLink data. Real-time attitude, GPS, battery, and flight status.
- **GCS:** ADOS Mission Control web application. Mission planning, drone configuration, flight controls, map overlay. Full ground control station in a browser tab.
- **Settings:** Ground station configuration (WiFi channel, TX power, video resolution, recording toggle) via web interface.

### What Does NOT Run in the Browser

- **Direct WFB-ng packet inspection.** The browser has no access to raw WiFi frames. All RF-level monitoring happens on the SBC.
- **USB device access.** You cannot plug a WiFi adapter into your laptop and use it through the browser for WFB-ng. The ground station SBC owns the adapter hardware.
- **Offline maps.** Map tiles need to be pre-cached on the SBC, or the ground station needs internet access (via a second WiFi adapter or Ethernet) to fetch tiles on demand.

## Access Methods for Drone Video

| Method | Latency | Range | Platform Support | Setup Effort |
|--------|---------|-------|-----------------|-------------|
| WFB-ng direct on a Linux host | 30-70 ms | 50 km+ | Linux only | CLI config, Linux knowledge |
| ADOS ground node, browser access | 50-80 ms | 50 km+ | Any browser | Plug and play |
| 4G/LTE relay | 100-300 ms | Cell coverage area | Any browser | Moderate |
| Standard WiFi | 20-50 ms | ~300 m | Any device | None |

The ground-node-plus-browser path adds ~10-20 ms over raw WFB-ng due to the mediamtx WebRTC relay step.

## Edge Cases and Known Limitations

| Scenario | Supported? | Notes |
|----------|-----------|-------|
| Multiple browsers viewing simultaneously | Yes | Multiple clients can connect to the WebRTC stream |
| Browser running in background (mobile) | Partial | iOS Safari and some Android browsers throttle background WebRTC. Keep the browser in foreground for reliable video |
| Split screen on iPad | Yes | Safari split view works normally |
| VPN active on client device | Yes | Traffic stays on local WiFi network between client and GS. VPN does not interfere |
| Cellular + GS WiFi at the same time | Depends | Some phones disconnect cellular when joining a WiFi network without internet. Disable auto-switch in your device settings if you need both |
| No internet connection | Yes | The ground station is fully self-contained. No internet needed for video, telemetry, or GCS |
| Multiple ground stations on same channel | No | WFB-ng uses broadcast, so multiple GS units on the same WiFi channel will interfere. Use different channels |
