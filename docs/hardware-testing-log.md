# Hardware Testing Log

> Record all test results here. One row per test. Never delete rows. Updated 2026-03-21.

## Test Categories

| Category | What to Measure | Pass Criteria |
|----------|----------------|---------------|
| Boot time | Power on to agent ready (REST API responding) | <15s (RK3566), <8s (RV1106) |
| MAVLink proxy | Serial read + WS/TCP relay, latency, throughput | <5ms added latency, 4 clients stable |
| Video encode | HW encode start, resolution, FPS, bitrate accuracy | Matches configured resolution and FPS |
| MQTT telemetry | Connect to broker, publish, verify message arrival | Messages arrive at configured Hz |
| WiFi AP | AP starts, SSID visible, client connects, DHCP works | Client gets IP, captive portal redirects |
| Memory usage | Total RAM after all services stable (5 min runtime) | Within estimates from memory-profiling.md |
| CPU usage | Idle %, encode %, full-load % | <80% sustained under full load |
| Thermal | CPU temp after 30 min runtime under load | <80C without active cooling |
| Stability | 1-hour continuous run, all services active | No crashes, no OOM, no memory growth |

## Measurement Commands

```bash
# Boot time (from serial console, measure time from power-on to this succeeding)
curl -s http://localhost:8080/api/status

# Memory
free -m
cat /proc/meminfo | grep -E "MemTotal|MemFree|MemAvailable|Buffers|Cached"

# CPU
top -bn1 | head -5
cat /proc/stat | head -1

# Temperature
cat /sys/class/thermal/thermal_zone0/temp    # Divide by 1000 for Celsius

# MAVLink latency (ping FC via pymavlink, measure round trip)
python3 -c "import pymavlink; ..."           # Custom test script TBD

# Disk
df -h /
```

---

## Procured Boards

| # | Board | Chip | RAM | Form Factor | Purchase Price | Status |
|---|-------|------|-----|-------------|---------------|--------|
| 1 | Luckfox Pico | RV1103 | 64 MB | 25.4x18mm | ~₹700 | Procured |
| 2 | Luckfox Pico Pro | RV1106 | 128 MB | 38x22mm | ~₹1,190 | Procured |
| 3 | Luckfox Pico Max | RV1106 | 256 MB | 38x22mm | ~₹2,400 | Procured |
| 4 | Luckfox Pico Zero | RV1106G3 | 256 MB | 45x26mm | ~₹2,400 | Procured |
| 5 | Luckfox Lyra | RK3506G2 | 256 MB | 65x30mm | ~₹1,350 | Procured |

---

## Test Results

### Board 1: Luckfox Pico (RV1103, 64MB)

| Date | Test | Result | Notes | Tester |
|------|------|--------|-------|--------|
| PENDING | Boot time | PENDING | | |
| PENDING | MAVLink proxy | PENDING | Expect this to work, 64MB is enough for MAVLink only | |
| PENDING | Video encode | PENDING | Expect FAIL due to 64MB RAM constraint | |
| PENDING | MQTT telemetry | PENDING | | |
| PENDING | WiFi AP | PENDING | Check if board has WiFi. If not, skip | |
| PENDING | Memory usage | PENDING | Critical test. Compare to 52MB estimate | |
| PENDING | CPU usage | PENDING | Single A7 core, watch for saturation | |
| PENDING | Thermal | PENDING | Low power chip, expect <50C | |
| PENDING | Stability (1hr) | PENDING | MAVLink + MQTT only (no video) | |

### Board 2: Luckfox Pico Pro (RV1106, 128MB)

| Date | Test | Result | Notes | Tester |
|------|------|--------|-------|--------|
| PENDING | Boot time | PENDING | | |
| PENDING | MAVLink proxy | PENDING | | |
| PENDING | Video encode | PENDING | Test at 720p@25 first, then try 1080p | |
| PENDING | MQTT telemetry | PENDING | | |
| PENDING | WiFi AP | PENDING | | |
| PENDING | Memory usage | PENDING | Critical test. Compare to 112MB estimate. 16MB free is tight | |
| PENDING | CPU usage | PENDING | Single core, watch during video encode | |
| PENDING | Thermal | PENDING | | |
| PENDING | Stability (1hr) | PENDING | Full stack at 720p. Watch for OOM | |

### Board 3: Luckfox Pico Max (RV1106, 256MB)

| Date | Test | Result | Notes | Tester |
|------|------|--------|-------|--------|
| PENDING | Boot time | PENDING | | |
| PENDING | MAVLink proxy | PENDING | | |
| PENDING | Video encode | PENDING | Test 1080p@25, H.264 and H.265 | |
| PENDING | MQTT telemetry | PENDING | | |
| PENDING | WiFi AP | PENDING | | |
| PENDING | Memory usage | PENDING | Compare to 135MB estimate. Should have ~121MB free | |
| PENDING | CPU usage | PENDING | | |
| PENDING | Thermal | PENDING | | |
| PENDING | Stability (1hr) | PENDING | Full stack at 1080p | |

### Board 4: Luckfox Pico Zero (RV1106G3, 256MB)

| Date | Test | Result | Notes | Tester |
|------|------|--------|-------|--------|
| PENDING | Boot time | PENDING | | |
| PENDING | MAVLink proxy | PENDING | | |
| PENDING | Video encode | PENDING | Same chip as Pico Max, expect similar results | |
| PENDING | MQTT telemetry | PENDING | | |
| PENDING | WiFi AP | PENDING | | |
| PENDING | Memory usage | PENDING | Should match Pico Max numbers | |
| PENDING | CPU usage | PENDING | | |
| PENDING | Thermal | PENDING | Smaller board, check if thermals differ | |
| PENDING | Stability (1hr) | PENDING | | |

### Board 5: Luckfox Lyra (RK3506G2, 256MB)

| Date | Test | Result | Notes | Tester |
|------|------|--------|-------|--------|
| PENDING | Boot time | PENDING | 3 cores should boot faster | |
| PENDING | MAVLink proxy | PENDING | Triple UART is a plus for this board | |
| PENDING | Video encode | PENDING | Expect FAIL. No HW encoder. Test SW x264 to confirm | |
| PENDING | MQTT telemetry | PENDING | | |
| PENDING | WiFi AP | PENDING | | |
| PENDING | Memory usage | PENDING | Compare to 67MB estimate (no video services) | |
| PENDING | CPU usage | PENDING | 3 cores, should be lower than single-core boards | |
| PENDING | Thermal | PENDING | | |
| PENDING | Stability (1hr) | PENDING | MAVLink + MQTT + API only (no video) | |

---

## Cross-Board Comparison (Fill After Testing)

| Metric | RV1103 64MB | RV1106 128MB | RV1106 256MB | RV1106G3 256MB | RK3506 256MB |
|--------|-------------|-------------|-------------|---------------|-------------|
| Boot time (s) | | | | | |
| RAM used (MB) | | | | | |
| RAM free (MB) | | | | | |
| CPU idle (%) | | | | | |
| CPU temp (C) | | | | | |
| Video encode | | | | | |
| Max FPS | | | | | |
| MAVLink latency (ms) | | | | | |
| 1hr stable | | | | | |

---

## Notes

- All tests should be run with the same FC (SpeedyBee F405 running ArduPilot 4.5.7) for consistency
- Use the same camera (OV5647 CSI or USB webcam) across all video tests
- Record ambient temperature at time of thermal test
- For stability tests, run with a MAVLink FC simulator (SITL over serial bridge) if no real FC is available
- Take screenshots of `htop` output and save to `docs/test-evidence/` for each board
