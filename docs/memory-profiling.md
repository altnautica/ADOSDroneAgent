# Memory Profiling — Per-Chip Footprint Analysis

> All values are ESTIMATES based on comparable deployments and published benchmarks. Actual measurements will be added after hardware testing. Updated 2026-03-21.

## Methodology

Memory estimates come from three sources:

1. Published Luckfox/Radxa documentation for kernel and system overhead
2. Measured Python process sizes from ADOS Drone Agent running on Raspberry Pi (Debian, aarch64)
3. Published memory usage for mediamtx, hostapd, dnsmasq, and Mosquitto

Buildroot-based systems (RV1103, RV1106, RK3506) use significantly less kernel and system memory than Debian-based systems (RK3566, RK3588S2). The estimates reflect this difference.

---

## RV1103 (64MB) — MARGINAL

**Verdict:** MAVLink + MQTT only. No video. No webapp. Bare minimum.

| Component | RAM (MB) | Notes |
|-----------|----------|-------|
| Linux kernel (Buildroot) | 8 | Minimal kernel, no modules |
| systemd (PID 1 only) | 2 | Or BusyBox init for less |
| Python 3.11 runtime | 12 | Interpreter + stdlib imports |
| MAVLink proxy | 8 | Serial parser + 4 client buffers (64KB each) |
| MQTT gateway | 5 | paho-mqtt + TLS context |
| Config loader (Pydantic) | 3 | Loaded once, models in memory |
| **Subtotal (core only)** | **38** | |
| Buffer / OS overhead | 14 | Page cache, slab, tmpfs |
| **Total used** | **~52** | |
| **Free** | **~12** | |

Cannot fit: FastAPI (~15MB), mediamtx (~25MB), hostapd+dnsmasq (~4MB), or config webapp static files.

**Recommendation:** Only viable as a "telemetry puck." If video is not needed, this works. But for $1-2 more, the RV1106 128MB includes video capability, making this chip hard to justify.

---

## RV1106 128MB — TIGHT

**Verdict:** Full stack fits, but ~16MB free is uncomfortable. One memory leak and it is OOM.

| Component | RAM (MB) | Notes |
|-----------|----------|-------|
| Linux kernel (Buildroot) | 8 | Minimal kernel |
| systemd | 2 | |
| Python 3.11 runtime | 12 | |
| MAVLink proxy | 8 | |
| MQTT gateway | 5 | |
| FastAPI + uvicorn | 15 | Single worker, no reload |
| mediamtx | 25 | WebRTC + RTSP, single stream |
| Video encode buffers (MPP) | 18 | 720p H.264, 3 frame buffers |
| hostapd + dnsmasq | 4 | WiFi AP + DHCP |
| Config / Pydantic | 3 | |
| **Subtotal** | **100** | |
| OS overhead | 12 | |
| **Total used** | **~112** | |
| **Free** | **~16** | |

Mitigations for tight memory:
- Use 720p@25fps instead of 1080p (saves ~6MB in encode buffers)
- Run uvicorn with `--limit-concurrency 4` to cap request memory
- Set Python `PYTHONDONTWRITEBYTECODE=1` to skip .pyc files
- Consider replacing FastAPI with a lighter HTTP server (aiohttp saves ~5MB)
- Set `vm.min_free_kbytes=4096` to prevent OOM before swap thrashing

---

## RV1106 256MB — COMFORTABLE

**Verdict:** Full ADOS Drone Agent stack with room to breathe. Primary cost-optimized target.

| Component | RAM (MB) | Notes |
|-----------|----------|-------|
| Linux kernel (Buildroot) | 8 | |
| systemd | 2 | |
| Python 3.11 runtime | 12 | |
| MAVLink proxy | 8 | |
| MQTT gateway | 5 | |
| FastAPI + uvicorn | 15 | |
| mediamtx | 25 | |
| Video encode buffers (MPP) | 25 | 1080p H.264, 3 frame buffers |
| hostapd + dnsmasq | 4 | |
| Config / Pydantic | 3 | |
| Captive portal (iptables) | 1 | Kernel-space, minimal |
| Config webapp (static) | 2 | Served from disk, cached pages |
| **Subtotal** | **110** | |
| OS overhead / page cache | 25 | |
| **Total used** | **~135** | |
| **Free** | **~121** | |

121MB free is enough for:
- Occasional memory spikes during WebRTC ICE negotiation (~10-15MB)
- Python garbage collection pressure
- Log buffering
- Future small features

This is the minimum chip where ADOS Drone Agent runs comfortably.

---

## RK3506 256MB — NOT RECOMMENDED

**Verdict:** 256MB is enough RAM, but no hardware video encoder makes this chip unsuitable.

| Component | RAM (MB) | Notes |
|-----------|----------|-------|
| Linux kernel (Buildroot) | 8 | |
| systemd | 2 | |
| Python 3.11 runtime | 12 | |
| MAVLink proxy | 8 | |
| MQTT gateway | 5 | |
| FastAPI + uvicorn | 15 | |
| hostapd + dnsmasq | 4 | |
| Config / Pydantic | 3 | |
| x264 SW encode (if attempted) | ~10 | CPU pegged at 100% on all 3 cores |
| **Subtotal** | **~67** | |
| **Free** | **~189** | |

Plenty of RAM. But software video encoding on three A7 cores at 1.5 GHz produces roughly 720p@10fps with 100% CPU usage. The agent cannot run alongside it. Without video, the RK3506 is just a more expensive RV1103.

---

## RK3566 2GB — RECOMMENDED

**Verdict:** Full stack with 1.7GB free. Room for future features, debugging, logging, and NPU inference.

| Component | RAM (MB) | Notes |
|-----------|----------|-------|
| Linux kernel (Debian) | 35 | Full kernel with modules |
| systemd + journald | 15 | Full systemd suite |
| Python 3.11 runtime | 15 | Debian Python, more stdlib |
| MAVLink proxy | 8 | |
| MQTT gateway | 5 | |
| FastAPI + uvicorn | 18 | Slightly larger on Debian |
| mediamtx | 28 | WebRTC + RTSP |
| Video encode buffers (MPP) | 30 | 1080p@30 H.264 |
| hostapd + dnsmasq | 5 | |
| Config / Pydantic | 3 | |
| Captive portal | 1 | |
| Config webapp | 2 | |
| D-Bus + NetworkManager | 8 | Debian default |
| SSH server (optional) | 3 | sshd if enabled |
| **Subtotal** | **176** | |
| OS overhead / page cache | 111 | Debian uses more page cache |
| **Total used** | **~287** | |
| **Free** | **~1,761** | |

1.7GB free means:
- NPU inference models can be loaded (~50-200MB for YOLOv5-nano)
- WFB-ng can run alongside (~15MB)
- Multiple WebRTC viewers (each adds ~5-10MB)
- Full debug logging to RAM
- pip install additional packages without worry
- Room for future features without memory anxiety

---

## RV1126B 256MB — COMFORTABLE

| Component | RAM (MB) | Notes |
|-----------|----------|-------|
| Linux kernel (Buildroot) | 10 | Slightly larger, ISP drivers |
| systemd | 2 | |
| Python 3.11 runtime | 12 | |
| MAVLink proxy | 8 | |
| MQTT gateway | 5 | |
| FastAPI + uvicorn | 15 | |
| mediamtx | 25 | |
| Video encode buffers (MPP) | 20 | H.265 uses less buffer than H.264 |
| ISP pipeline | 8 | Dual-ISP hardware |
| hostapd + dnsmasq | 4 | |
| Config / Pydantic | 3 | |
| Captive portal + webapp | 3 | |
| **Subtotal** | **115** | |
| OS overhead | 20 | |
| **Total used** | **~135** | |
| **Free (256MB)** | **~121** | |
| **Free (1GB variant)** | **~889** | |

The 256MB variant matches the RV1106 256MB profile. The 1GB variant is excellent, providing nearly 900MB free for NPU models and future features.

H.265 encoding actually uses slightly less buffer memory than H.264 at the same resolution because the compressed frame references are smaller.

---

## Summary Comparison

| Chip | Total RAM | ADOS Drone Agent Usage | Free | Rating |
|------|-----------|-----------------|------|--------|
| RV1103 | 64 MB | ~52 MB (core only) | ~12 MB | Marginal |
| RV1106 128MB | 128 MB | ~112 MB (full stack) | ~16 MB | Tight |
| RV1106 256MB | 256 MB | ~135 MB (full stack) | ~121 MB | Comfortable |
| RK3506 256MB | 256 MB | ~67 MB (no video) | ~189 MB | Not Recommended |
| RK3566 2GB | 2,048 MB | ~287 MB (full stack) | ~1,761 MB | Recommended |
| RV1126B 256MB | 256 MB | ~135 MB (full stack) | ~121 MB | Comfortable |
| RV1126B 1GB | 1,024 MB | ~135 MB (full stack) | ~889 MB | Strong |

---

## Measurement Plan

Once dev boards arrive, measure actual memory with:

```bash
# Total system memory after boot (before agent)
free -m

# Agent process memory (after starting ados)
ps aux | grep ados
cat /proc/$(pidof python3)/status | grep VmRSS

# mediamtx memory
cat /proc/$(pidof mediamtx)/status | grep VmRSS

# System-wide breakdown
cat /proc/meminfo

# Per-process sorted by RSS
ps -eo pid,rss,comm --sort=-rss | head -20
```

Record all measurements in `hardware-testing-log.md` under the "Memory usage" test category.
