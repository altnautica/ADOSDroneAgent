# Ground Station Competitive Landscape

How the ADOS Ground Station compares to existing options for long-range drone video and control.

## Market Overview

No commercial turnkey WFB-ng ground station exists at the $100-350 price point. OpenHD and RubyFPV are free and open-source but require DIY Linux assembly. Commercial controllers (SIYI, Herelink, Skydroid) use proprietary radio links, cost $400-1,500, and have higher latency (180-200ms vs WFB-ng's 30-70ms).

## Comparison Matrix

| Feature | ADOS GS Lite | ADOS GS Pro | OpenHD (DIY) | RubyFPV (DIY) | SIYI MK32 | Herelink v1.1 | Skydroid G12 |
|---------|-------------|-------------|-------------|---------------|-----------|--------------|-------------|
| **Type** | SBC + adapter | SBC + dual adapter | Pi + adapter | Pi + adapter | All-in-one RC | All-in-one RC | All-in-one RC |
| **Price** | $100-150 | $200-350 | $130-200 BOM | $130-200 BOM | $1,112 bundle | $800-1,500 | $400-700 |
| **Setup** | Plug and play | Plug and play | DIY Linux | DIY Linux | Out of box | Out of box | Out of box |
| **Range** | 5-50 km | 50 km+ | 50 km+ | 50 km+ | 15 km | 20 km | 20 km |
| **Video Latency** | 30-70 ms | 30-70 ms | 100-150 ms | 32-70 ms | ~180 ms | ~200 ms | ~180 ms |
| **Video Output** | WebRTC (browser) | WebRTC + HDMI | QOpenHD app | RubyFPV app | Built-in 7" screen | Built-in 5.46" screen | Built-in 5.5" screen |
| **RC Built-in** | No | No | No | Yes (optional) | Yes | Yes | Yes |
| **GCS** | ADOS MC (browser) | ADOS MC (browser) | QOpenHD / QGC | RubyFPV app | SIYI FPV (Android) | Solex / QGC (Android) | QGC (Android) |
| **Mac Support** | Yes (browser) | Yes (browser) | No | No | No | No | No |
| **Windows Support** | Yes (browser) | Yes (browser) | No | No | No | No | No |
| **iOS Support** | Yes (browser) | Yes (browser) | No | No | No | No | No |
| **Android Support** | Yes (browser) | Yes (browser) | Android only | No | Android only | Android only | Android only |
| **Linux Support** | Yes (native + browser) | Yes (native + browser) | Yes | Yes | No | No | No |
| **ChromeOS Support** | Yes (browser) | Yes (browser) | No | No | No | No | No |
| **Open Source** | Yes (GPLv3) | Yes (GPLv3) | Yes (GPLv2) | Yes (GPLv3) | No | No | No |
| **Diversity RX** | No | Yes (dual adapter) | Yes | Yes | N/A | N/A | N/A |
| **ArduPilot** | Yes | Yes | Yes | Yes | Yes | Yes | Limited |
| **PX4** | Planned | Planned | Partial | No | Yes | Yes | No |

## Detailed Competitor Profiles

### OpenHD

- **Project:** https://github.com/OpenHD/OpenHD
- **License:** GPLv2
- **Hardware:** Raspberry Pi 3/4/5 + RTL8812AU/EU
- **GCS app:** QOpenHD (Qt-based, Linux and Android)
- **Strengths:** Large community, mature codebase, supports Pi Camera natively, good documentation
- **Weaknesses:** Linux-only ground station, no browser interface, 100-150ms latency (higher than raw WFB-ng due to OSD overlay processing pipeline), complex initial setup for users unfamiliar with Linux

### RubyFPV

- **Project:** https://rubyfpv.com
- **License:** GPLv3
- **Hardware:** Raspberry Pi + RTL8812AU/EU
- **GCS app:** Custom Ruby app (Linux only)
- **Strengths:** Lowest latency in the WFB-ng ecosystem (32-70ms), adaptive bitrate control, relay node support, optional RC-over-WFB-ng
- **Weaknesses:** Linux-only, Pi-only, single developer project, closed development process, no browser-based access, no Mac or Windows viewer

### SIYI MK32

- **Company:** SIYI Technology (Shenzhen)
- **Hardware:** All-in-one handheld with 7" touchscreen, dual gimbal RC sticks
- **Radio:** Proprietary mesh radio, 2.4GHz
- **Range:** 15 km
- **Latency:** ~180 ms
- **Strengths:** Polished industrial design, built-in display and RC, Android GCS app, good for commercial operators who want an all-in-one unit
- **Weaknesses:** $1,112 price tag, closed proprietary ecosystem, Android-only GCS, shorter range than WFB-ng, higher latency

### Herelink v1.1

- **Company:** Hex/ProfiCNC (Australia)
- **Hardware:** Controller + air unit pair, 5.46" 1000-nit touchscreen
- **Radio:** Proprietary, 2.4GHz
- **Range:** 20 km
- **Latency:** ~200 ms
- **Strengths:** Deep ArduPilot ecosystem integration (Hex is a major ArduPilot hardware partner), bright outdoor display, Solex and QGC apps, popular with commercial operators
- **Weaknesses:** $800-1,500 depending on configuration, heavy, Android-only, closed radio protocol, high latency

### Skydroid G12

- **Company:** Skydroid (Shenzhen)
- **Products:** H16, G12, T12 (various form factors and price points)
- **Radio:** Proprietary, 2.4GHz
- **Range:** 10-20 km depending on model
- **Latency:** ~180 ms
- **Strengths:** Multiple form factors to choose from, IP67 weather sealing on some models, reasonable price for a commercial controller
- **Weaknesses:** Closed ecosystem, limited firmware and protocol support, Android-only GCS, inconsistent build quality reported by some users

## ADOS Advantages

**1. Cheapest turnkey ground station.** The Lite configuration costs $100-150 assembled and tested. Commercial alternatives start at $400 (Skydroid) and go past $1,000 (SIYI, Herelink). OpenHD and RubyFPV have similar BOM costs but ship as a pile of parts with a wiki.

**2. Lowest video latency.** WFB-ng with hardware H.264 encoding delivers 30-70ms glass-to-glass latency. The mediamtx WebRTC relay adds about 10-20ms. Commercial systems like Herelink and SIYI run at 180-200ms. That 100-150ms difference is noticeable during manual FPV flight and time-critical inspection work.

**3. Longest range at this price point.** 50km+ with a directional antenna, using the same WFB-ng protocol proven by OpenHD and RubyFPV. Commercial systems cap at 15-20km unless you move into enterprise-grade equipment at much higher prices.

**4. Works on any OS via browser.** Connect your laptop, phone, or tablet over WiFi. Open Chrome, Firefox, Safari, or Edge. That's it. macOS, Windows, iOS, Android, Linux, ChromeOS. Every competitor either requires a specific app (Android-only for Herelink, SIYI, Skydroid) or a specific OS (Linux-only for OpenHD and RubyFPV ground station software).

**5. No app installation required.** WebRTC video and the ADOS Mission Control GCS run entirely in the browser. No Play Store download, no APK sideloading, no pip install, no driver setup. Open a URL and you're connected.

**6. Fully open source.** Hardware schematics, agent software, and ground station code are GPLv3. You can inspect, modify, rebuild, and redistribute. Commercial systems are closed boxes. If a firmware update breaks something, you wait for the vendor. With open source, you fix it yourself or the community does.

**7. GCS included.** ADOS Mission Control provides mission planning, telemetry display, drone configuration, and video in one browser tab. OpenHD and RubyFPV provide video only. You still need a separate GCS (Mission Planner or QGroundControl) running on another device, connected through yet another telemetry link.

**8. Same baseboard as the air unit.** The ground station and air unit share the same hardware design. Configure it for TX mode (air) or RX mode (ground) via software. One board to stock, one set of spares. This reduces manufacturing cost and simplifies inventory for OEMs and operators.

## ADOS Limitations

These are real trade-offs, not marketing spin.

**No built-in RC transmitter.** The ADOS ground station handles video and telemetry only. You need a separate RC transmitter for manual flight control. ExpressLRS ($30-50 for TX module) or Crossfire ($50-80) are the typical choices. Most FPV pilots already own RC gear, but commercial operators switching from Herelink or SIYI will need to source an RC link separately.

**No display on Lite model.** The Lite ground station is a headless SBC. You view video on your own device (laptop, phone, tablet) via browser. This is actually a feature for many users (your existing device has a better screen than a $400 controller's built-in display), but operators who want a dedicated monitor need the Pro variant with HDMI output or need to bring their own screen.

**Requires separate RC link for manual flight.** Autonomous missions work fine over MAVLink commands through the ground station. But for stick-flying, you need a dedicated RC link (ExpressLRS recommended). This adds $30-80 to total ground equipment cost and another piece of hardware to manage.

**Half-duplex WFB-ng.** The current WFB-ng implementation is half-duplex on a single channel. Video flows down (air to ground) and telemetry flows up (ground to air), sharing bandwidth on the same frequency. In practice, telemetry uses very little bandwidth compared to video, so this rarely causes issues. But it's a protocol-level limitation of WFB-ng, not something ADOS can work around without modifying the protocol.

## Why Not Just Use OpenHD or RubyFPV?

OpenHD and RubyFPV are excellent open-source projects. They proved that WFB-ng-based video links work at 50km+ range with sub-100ms latency. The ADOS ground station uses the same underlying protocol. Fair question: why build another product?

**Assembly is the first barrier.** You need to source a Raspberry Pi (often out of stock), find a compatible RTL8812AU or EU adapter (many variants have hardware revisions that break driver compatibility), flash a custom Linux image to an SD card, SSH in, configure WFB-ng parameters, set up the camera pipeline, wire power, and put it all in a case. The documentation covers most of this, but troubleshooting when something doesn't work requires Linux knowledge. Kernel driver issues, permission problems, and config file syntax errors are common for first-time builders.

**No browser-based access.** OpenHD uses QOpenHD, a Qt-based application that runs on Linux and Android. RubyFPV has its own Linux-native application. Neither supports viewing video in a web browser. If you're on a Mac laptop in the field (roughly 35% of drone operators), you cannot see your video feed without running a Linux VM or carrying a separate Android device.

**No Mac or Windows ground station support.** Both projects require Linux on the ground station SBC, and their viewer applications are also Linux-native (QOpenHD has an Android port). There is no official way to run the ground station software on macOS or Windows. The ADOS ground station runs Linux on the SBC internally but serves everything over WiFi and WebRTC to any browser on any OS.

**No turnkey product exists.** OpenHD and RubyFPV are community-driven projects, not commercial products. There is no company offering assembled units, guaranteed compatibility lists, warranties, or technical support. This is fine for hobbyists and tinkerers who enjoy building their own gear. It's a problem for commercial operators who need a reliable tool that works every time they power it on. Debugging kernel driver conflicts in the field is not an acceptable workflow for a surveying company or an inspection firm.

The ADOS ground station stands on the work done by the WFB-ng, OpenHD, and RubyFPV communities. It packages the same proven radio protocol into a product that works for people who don't want to become Linux system administrators to fly a drone.

## Price Positioning

| Segment | Products | Price Range | ADOS Position |
|---------|----------|------------|--------------|
| DIY open source | OpenHD, RubyFPV | $130-200 (parts) | Similar BOM cost, but turnkey |
| Budget commercial | Skydroid H16 | $300-500 | 2-3x cheaper |
| Mid commercial | SIYI MK15, Skydroid G12 | $400-700 | 3-5x cheaper |
| Premium commercial | SIYI MK32, Herelink v1.1 | $800-1,500 | 5-10x cheaper |
| Enterprise | Custom integrations | $2,000+ | Different market segment |
