# Companion Computer Chip Comparison

> All chips evaluated for ADOS ADOS Drone Agent. Updated 2026-03-21.

## Summary Table

| Chip | Arch | Cores | Speed | RAM Options | NPU | Video Encode | Video Decode | Chip Price | Board Price | Viability |
|------|------|-------|-------|-------------|-----|-------------|-------------|-----------|------------|-----------|
| RV1103 | Cortex-A7 | 1 | 1.2 GHz | 64MB (PoP) | 0.5 TOPS | H.264 1080p | H.264 1080p | ~$2-3 | ~₹700 (Luckfox Pico) | Marginal |
| RV1106 | Cortex-A7 | 1 | 1.2 GHz | 128-256MB (PoP) | 0.5 TOPS | H.264/H.265 1080p | H.264/H.265 | ~$3-5 | ₹1,190-2,400 (Luckfox) | Viable (256MB) |
| RV1106G3 | Cortex-A7 | 1 | 1.2 GHz | 256MB (PoP) | 0.5 TOPS | H.264/H.265 | H.264/H.265 | ~$4-5 | ~₹2,400 (Luckfox Pico Zero) | Viable |
| RK3506G2 | Cortex-A7 | 3 | 1.5 GHz | 256MB (PoP) | None | None (decode only) | H.264 | ~$3-4 | ~₹1,350 (Luckfox Lyra) | Not Recommended |
| RK3566 | Cortex-A55 | 4 | 1.8 GHz | 1-8GB | 0.8 TOPS | H.264 1080p60 | H.264/H.265 4K | ~$8-12 | $36 (Radxa CM3) | **RECOMMENDED** |
| RV1126B | Cortex-A7 | 4 | 1.5 GHz | 256MB-1GB | 2.0 TOPS | H.265 4K30 | H.264/H.265 | ~$8-10 | Not available in India | Strong |
| RK3576 | A72+A53 | 4+4 | 2.2+1.8 GHz | 2-16GB | 6 TOPS | H.265 4K60 | 8K | ~$15-20 | $70 (Radxa CM4) | **ACTIVE** (Android RC) |
| RK3588S2 | A76+A55 | 4+4 | 2.4+1.8 GHz | 4-32GB | 6 TOPS | H.265 8K30 | 8K | ~$35-40 | $72 (Radxa CM4) | High end |

---

## Per-Chip Analysis

### RV1103 (Luckfox Pico)

**Viability: MARGINAL**

- **Memory:** 64MB PoP (Package on Package, soldered to chip). After kernel + systemd, roughly 40MB free. Python runtime alone takes 12-15MB. Running MAVLink proxy + MQTT is possible. Adding video is not.
- **Video Encode:** H.264 1080p via Rockchip MPP. Hardware is capable, but 64MB RAM cannot hold the encode buffers alongside the agent.
- **Power:** ~0.3-0.5W typical. Excellent for battery life.
- **NPU:** 0.5 TOPS. Could run tiny inference models (gesture detection, simple object tracking). Not useful at 64MB RAM.

**Pros:**
- Cheapest option (~₹700 for a complete board)
- Tiny form factor (25.4 x 18mm on Luckfox Pico)
- Extremely low power draw
- CSI camera input available

**Cons:**
- 64MB is a hard wall. No video streaming possible alongside the agent
- Single A7 core, 1.2 GHz. Python asyncio will feel the squeeze
- No USB host on most board variants (limits peripherals)
- Buildroot only (no Debian/Ubuntu support)

**Recommended Use:** MAVLink-to-MQTT bridge only (no video). A "telemetry puck" that sends position and status to the cloud. Not a full ADOS Drone Agent deployment.

**Memory Budget:** See `memory-profiling.md` for detailed breakdown.

---

### RV1106 (Luckfox Pico Pro / Pico Max)

**Viability: VIABLE (256MB), TIGHT (128MB)**

- **Memory:** 128MB or 256MB PoP. The 256MB variant can run the full ADOS Drone Agent stack (MAVLink + video + MQTT + API + WiFi AP) with ~120MB to spare. The 128MB variant works but leaves only ~16MB free, which is uncomfortable.
- **Video Encode:** H.264 and H.265 at 1080p via MPP. The H.265 support is a real advantage for bandwidth-constrained 4G links (50% bitrate savings over H.264).
- **Power:** ~0.4-0.7W typical. Battery-friendly.
- **NPU:** 0.5 TOPS. Same as RV1103. Limited practical use at this tier.

**Pros:**
- Very cheap (₹1,190 for 128MB, ₹2,400 for 256MB)
- H.265 hardware encode (rare at this price point)
- Small form factor
- Built-in ISP (Image Signal Processor) for camera processing
- Good Buildroot support from Luckfox
- Ethernet available on Pro/Max boards

**Cons:**
- Single A7 core. One CPU-bound task blocks everything
- 128MB variant is too tight for comfort
- Buildroot only, no Debian
- Limited USB (one host port on most boards)
- No PCIe, no SATA, no display output

**Recommended Use:** Cost-optimized ADOS Drone Agent deployment. Best at 256MB. Good for basic companion computer duties: video streaming, telemetry relay, config webapp.

---

### RV1106G3 (Luckfox Pico Zero)

**Viability: VIABLE**

- **Memory:** 256MB PoP. Same as RV1106 256MB variant.
- **Video Encode:** H.264/H.265 via MPP. Same silicon as RV1106.
- **Power:** ~0.4-0.6W typical.

**Pros:**
- Ultra-compact form factor (Luckfox Pico Zero is credit-card sized)
- Same capabilities as RV1106 256MB
- Good CSI camera support

**Cons:**
- Same single-core limitation as RV1106
- Limited I/O compared to Pro/Max boards
- Buildroot only

**Recommended Use:** Same as RV1106 256MB. Choose based on form factor requirements.

---

### RK3506G2 (Luckfox Lyra)

**Viability: NOT RECOMMENDED**

- **Memory:** 256MB PoP. Enough RAM for the agent, but the chip has a fatal flaw.
- **Video Encode:** **None.** The RK3506 has a hardware H.264 decoder but NO encoder. Software encoding (x264/ffmpeg) on three A7 cores at 1.5 GHz would max out all CPUs at 720p@15fps and leave nothing for the agent.
- **Power:** ~0.5-0.8W typical.
- **NPU:** None.

**Pros:**
- Three A7 cores (more CPU than RV1103/RV1106)
- Cheap (~₹1,350 for Luckfox Lyra board)
- Ethernet + USB host
- Triple UART (good for MAVLink + GPS + spare)

**Cons:**
- No hardware video encoder. This is a dealbreaker for a companion computer
- No NPU
- 256MB RAM would be fine, but without video there is no reason to pick this over RV1106

**Recommended Use:** Not recommended for ADOS Drone Agent. Could work as a headless MAVLink-to-MQTT bridge if you genuinely do not need video, but the RV1106 costs similar and includes a hardware encoder.

---

### RK3566 (Radxa CM3)

**Viability: RECOMMENDED**

- **Memory:** 1GB, 2GB, 4GB, or 8GB LPDDR4. The 2GB variant is the sweet spot.
- **Video Encode:** H.264 at 1080p@60fps via Rockchip MPP. Solid and well-tested.
- **Video Decode:** H.264/H.265 at 4K. Useful if the agent ever needs to consume video.
- **Power:** ~1.5-3W typical (varies with load and RAM config).
- **NPU:** 0.8 TOPS. Can run MobileNet, YOLOv5-nano. Useful for basic object detection.
- **GPU:** Mali G52 2EE. Not needed for headless operation, but available.

**Pros:**
- 4x Cortex-A55 cores at 1.8 GHz. Genuine multi-core performance
- 2GB RAM leaves 1.7GB free after ADOS Drone Agent. Room for future features
- Well-supported by Radxa (Debian, Ubuntu, Buildroot, mainline kernel progress)
- CM3 form factor (100-pin Hirose connectors). Same connector layout proposed for the reference companion baseboard
- PCIe 2.1, USB 3.0, dual CSI, HDMI. Real I/O
- Proven in production (Radxa ships thousands of CM3 units)
- Available in India
- Runs full Debian with apt. Much easier development than Buildroot-only chips

**Cons:**
- Higher power draw than RV1106 (~2W vs ~0.5W)
- Larger module size (55mm x 40mm CM3 vs tiny Luckfox boards)
- No H.265 encoder (H.264 only)
**Recommended Use:** Primary target for the reference companion baseboard. Best balance of performance, memory, I/O, and software support. The Cortex-A55 cores handle Python asyncio without breaking a sweat. 2GB RAM means no memory anxiety. Debian support means pip install works.

---

### RV1126B

**Viability: STRONG (if available)**

- **Memory:** 256MB to 1GB (depending on variant). The 1GB version is ideal.
- **Video Encode:** H.265 at 4K@30fps via MPP. Best video encoding at this price tier.
- **Power:** ~1-2W typical.
- **NPU:** 2.0 TOPS. Meaningful inference capability (YOLOv5s, MobileNetV3-Large, face detection).
- **ISP:** Built-in dual-ISP. Direct camera processing without external ISP chip.

**Pros:**
- 4x Cortex-A7 at 1.5 GHz. Better multi-core than RV1106
- H.265 4K hardware encode. Superior video capability
- 2.0 TOPS NPU. Enables on-device AI features (person detection, tracking)
- Built-in ISP handles HDR, WDR, 3D NR
- Chip price is competitive with RK3566

**Cons:**
- **Not readily available in India.** This is the primary blocker
- Sourcing for Indian production is uncertain
- A7 cores (not A55). Weaker per-core performance than RK3566
- Limited board ecosystem (fewer dev boards than RK3566)
- Buildroot preferred (Debian support is patchy)

**Recommended Use:** If the OEM partner handles hardware and manufacturing, this is a strong pick. For independent or India-side prototyping, the RK3566 is more practical due to availability.

---

### RK3576

**Viability: ACTIVE. Android RC controller target.**

Confirmed running Android with 4GB RAM + 64GB Flash. Target SoC for the ADOS Android RC controller product. NOT for companion computer; companion uses RV1126B.

- **Memory:** 2-16GB LPDDR5. Reference config: 4GB RAM + 64GB Flash.
- **Video Encode:** H.265 at 4K@60fps.
- **Power:** ~3-5W typical.
- **NPU:** 6 TOPS. Serious inference capability.
- **Dev Board:** Radxa CM4 (RK3576) — $70 for 4GB/32GB config.

**Pros:**
- big.LITTLE (4x A72 + 4x A53). Desktop-class performance
- 6 TOPS NPU. Real AI at the edge
- H.265 4K60 encode
- LPDDR5 bandwidth
- Android 14 support (critical for RC controller use case)
- Radxa CM4 available as dev board ($70 for 4GB/32GB)

**Cons:**
- Higher power draw and thermal requirements (acceptable for handheld RC with battery)
- Overkill for basic companion computer duties (use RV1126B instead)

**Recommended Use:** Android RC controller primary target. Runs ADOS Mission Control as native Android GCS app with touchscreen, physical sticks, and video display. Companion computer uses RV1126B instead (lower power, better ISP). Not needed for air-side ADOS Drone Agent.

---

### RK3588S2 (Radxa CM4)

**Viability: HIGH END**

- **Memory:** 4GB, 8GB, 16GB, or 32GB LPDDR5.
- **Video Encode:** H.265 at 8K@30fps. Three independent encode streams.
- **Power:** ~5-8W typical.
- **NPU:** 6 TOPS (3x 2T NPU cores).

**Pros:**
- Most powerful option. 4x A76 + 4x A55, 2.4 GHz
- Multi-stream video (encode 3 cameras simultaneously)
- 6 TOPS NPU for real AI workloads
- Same Hirose connector as CM3 (same baseboard works)
- Well-supported by Radxa (Debian, Ubuntu, Android)
- Available in India

**Cons:**
- 5-8W power draw requires active cooling or heatsink
- Overkill for basic ADOS Drone Agent
- Higher thermal output in enclosed drone housing

**Recommended Use:** High-end companion computer for customers who need multi-camera, AI inference, and full ROS 2. Not the initial ADOS Drone Agent target. Same baseboard as RK3566 (CM3 connector compatible with CM4).

---

## Decision Matrix

| Criteria | RV1103 | RV1106 256MB | RK3506 | RK3566 2GB | RV1126B 1GB | RK3588S2 |
|----------|--------|-------------|--------|-----------|-------------|----------|
| Full ADOS Drone Agent | No | Yes | No | Yes | Yes | Yes |
| Video HW Encode | Yes* | Yes | No | Yes | Yes | Yes |
| H.265 Encode | No | Yes | No | No | Yes | Yes |
| RAM Headroom | None | Moderate | N/A | Large | Moderate | Huge |
| India Availability | Yes | Yes | Yes | Yes | No | Yes |
| Board Cost | ₹700 | ₹2,400 | ₹1,350 | $36 | N/A | $72 |
| Power Draw | 0.3W | 0.5W | 0.6W | 2W | 1.5W | 6W |
| Debian Support | No | No | No | Yes | Partial | Yes |
| NPU | 0.5T | 0.5T | None | 0.8T | 2.0T | 6T |

*RV1103 can encode video but 64MB RAM prevents running the encoder alongside the agent.

## Ground Station Suitability

Ground station RX mode needs LESS compute than air unit TX mode. No video encoding is required. The ground station runs wfb_rx to decode the incoming WFB-ng stream and mediamtx to relay it as WebRTC to browsers.

| Chip | RAM | GS Suitability | Notes |
|------|-----|---------------|-------|
| RV1103 | 64MB | Not suitable | Not enough RAM for mediamtx WebRTC relay |
| RV1106 | 128MB | Marginal | Can run wfb_rx only, no WebRTC relay |
| RV1106 | 256MB | Viable (GS Lite) | wfb_rx + mediamtx works, single adapter |
| RK3506 | 256MB | Viable | No video encode needed in RX mode, just relay. The missing encoder is not a problem here |
| RK3566 | 2GB | **Recommended (GS Lite)** | Plenty of RAM for wfb_rx + mediamtx + WiFi AP + config webapp |
| RV1126B | 256MB-1GB | Good | Overkill for GS duties, but works well |
| RK3588S2 | 4GB | **Recommended (GS Pro)** | Dual RTL8812EU diversity, HDMI output, USB 3.0 for adapter bandwidth |

---

## Recommendation

**Primary target: RK3566 (Radxa CM3, 2GB).** Best balance of capability, availability, and developer experience. Debian support and 2GB RAM make development fast. Same connector as RK3588S2 allows a single baseboard design for both Lite and Pro tiers.

**Cost-optimized fallback: RV1106 256MB.** For OEMs who need the absolute lowest BOM cost and accept the single-core limitation.

**OEM-partner track: RV1126B 1GB.** If the OEM partner handles sourcing and manufacturing, this chip has the best video and NPU for its price. Altnautica cannot prototype on it without hardware.
