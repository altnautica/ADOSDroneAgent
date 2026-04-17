# WFB-ng Technical Guide

## What is WFB-ng?

WFB-ng (WiFi Broadcast, Next Generation) is an open-source long-range packet radio link. It puts standard WiFi adapters into monitor mode for raw packet transmission, bypassing the normal 802.11 protocol stack. No association, no ACK handshake, no range-limiting timeouts. The result: 5-50km range at 30-70ms latency using $30-50 off-the-shelf hardware.

Project: https://github.com/svpcom/wfb-ng

## How It Works

1. WiFi adapter enters monitor mode (raw packet injection/capture).
2. Transmitter (`wfb_tx`) injects packets directly at the link layer.
3. No 802.11 association means no ACK timeout, which means no protocol-imposed range limit.
4. FEC (Forward Error Correction) handles packet loss instead of retransmission.
5. Encryption via pre-shared keypair (mandatory).
6. Half-duplex: TX and RX share bandwidth on the same frequency.

## WFB-ng vs Standard WiFi

| Aspect | Standard WiFi | WFB-ng |
|--------|--------------|--------|
| Connection | Client associates with AP | Pure broadcast, no association |
| Range | ~300m (ACK timeout limit) | 5-50km+ (no ACK needed) |
| Error handling | Retransmissions (variable latency) | FEC (fixed latency) |
| Latency | 10-100ms+ (retx overhead) | 30-70ms (consistent) |
| Encryption | WPA2/3 | Custom keypair |
| Devices | Any WiFi device | Requires monitor mode driver |
| Bandwidth | Full duplex | Half duplex (shared TX/RX) |
| Reliability | Guaranteed delivery | Best effort + FEC |

## RTL8812EU Adapter Specs

| Spec | Value |
|------|-------|
| Chipset | Realtek RTL8812EU |
| TX Power | 29dBm (800mW) |
| Frequency | 5.8GHz (802.11ac) |
| Bandwidth | 20/40 MHz |
| MIMO | 2T2R |
| Interface | USB 2.0 (module) or USB-C (dongle) |
| VID:PID | 0BDA:B812 |
| Driver | Patched rtl8812eu (svpcom fork) |
| Module | LB-LINK BL-M8812EU2 (30x30mm, ~$10) |

**IMPORTANT:** RTL8812BU is a DIFFERENT chip. It does NOT support monitor mode. Do not confuse them. The names differ by one letter but the silicon is fundamentally different.

## Forward Error Correction (FEC)

WFB-ng uses Reed-Solomon FEC to recover lost packets without retransmission. This keeps latency fixed regardless of packet loss.

| Parameter | Default | Description |
|-----------|---------|-------------|
| K (data blocks) | 8 | Number of original data packets per FEC block |
| N (total blocks) | 12 | Total packets including parity (N-K = 4 parity) |
| Loss tolerance | 33% | Can lose 4 of 12 packets and still decode |
| Overhead | 50% | 4 extra packets per 8 data = 50% bandwidth overhead |
| Latency impact | Low | Smaller K = lower latency but less protection |

### FEC Tuning Profiles

| Profile | K | N | Loss Tolerance | Overhead | Use Case |
|---------|---|---|---------------|----------|----------|
| Aggressive (low latency) | 4 | 8 | 50% | 100% | Racing, close range |
| Default (balanced) | 8 | 12 | 33% | 50% | General flying |
| Conservative (long range) | 8 | 16 | 50% | 100% | Max range, noisy RF |

Smaller K means lower latency (fewer packets to collect before decoding) but requires more overhead to maintain the same loss tolerance.

## Encryption and Key Management

| Step | Detail |
|------|--------|
| Generation | `wfb_keygen` produces two key files |
| `drone.key` | TX private key + GS public key (goes on air unit) |
| `gs.key` | GS private key + TX public key (goes on ground station) |
| Algorithm | NaCl crypto_box (Curve25519 + XSalsa20 + Poly1305) |
| Key mismatch symptom | "Unable to decrypt session key" error in wfb_rx logs |

Keys must match between air unit and ground station. Factory provisioning pre-generates keypairs per device pair. The QR code on the air unit label encodes the pairing key for easy ground station setup.

## Channel and Frequency

| Channel | Frequency | Band | Notes |
|---------|-----------|------|-------|
| 36 | 5180 MHz | U-NII-1 | Indoor only in some regions |
| 149 | 5745 MHz | U-NII-3 | Good, less interference |
| 157 | 5785 MHz | U-NII-3 | Good |
| 161 | 5805 MHz | U-NII-3 | Default, widely available |
| 165 | 5825 MHz | U-NII-3 / ISM | Most permissive globally |

Default: channel 161 (5805 MHz), 20 MHz bandwidth.

Pre-flight checklist: run `iw scan` to find the least-congested channel before flight. Urban environments have significant WiFi interference on lower channels.

## Performance Envelope

| Antenna (TX) | Antenna (RX) | Range (reliable) | Range (max) |
|-------------|-------------|-----------------|-------------|
| Omni 5dBi | Omni 5dBi | 10-15 km | ~23 km |
| Omni 5dBi | Patch 12dBi | 25-35 km | ~40 km |
| Omni 5dBi | Yagi 16dBi | 40-55 km | ~60 km |

These numbers assume clear line of sight, 20MHz bandwidth, default FEC (8/12), and RTL8812EU at 20dBm TX power.

## Latency Breakdown

| Stage | Time |
|-------|------|
| Camera capture | 8-16ms |
| H.264 HW encode | 5-15ms |
| Packetization + FEC | 1-3ms |
| RF transmission | <1ms |
| RF reception | <1ms |
| FEC decode | 1-3ms |
| H.264 decode (browser) | 5-20ms |
| Display render | 8-16ms |
| **Total** | **30-70ms typical** |

The biggest variable is camera capture latency (depends on sensor rolling shutter) and browser decode (depends on device GPU).

## Configuration File

```yaml
wfb:
  mode: tx           # tx (air unit) or rx (ground station)
  enabled: false     # auto-enabled when RTL8812EU detected
  channel: 161       # 5.8GHz (5805MHz)
  bandwidth: 20      # MHz (20 or 40)
  tx_power: 20       # dBm (max 29 for RTL8812EU, software-limited per region)
  fec_k: 8           # data blocks
  fec_n: 12          # total blocks
  key_file: /etc/ados/wfb.key
```

## Distributed Receive Primitives

WFB-ng already ships the forwarder and aggregator flags needed to split the receiver across multiple physical nodes. The agent wraps them behind `ground_station.role`.

| Role | Command the agent runs | What it does |
|------|---|---|
| `direct` | `wfb_rx -p <port> -u <udp-out> -K <keyfile> <monitor-iface>` | Single-node receive. Decodes WFB-ng, decrypts, runs FEC, emits video on a UDP port that mediamtx picks up. |
| `relay` | `wfb_rx -f <receiver-ip> -p <port> -K <keyfile> <monitor-iface>` | Same decode and decrypt, but instead of emitting locally the relay forwards surviving fragments to the receiver over the mesh. |
| `receiver` | `wfb_rx -a -p <port> -u <udp-out> -K <keyfile> <monitor-iface>` | Aggregator mode. Accepts fragments from its own monitor adapter AND from every forwarder that can reach it. Reed-Solomon FEC combine works across the merged stream. |

**FEC combine.** With `wfb_rx -a`, the receiver runs the same k=8 / n=12 FEC on the union of fragments it heard locally plus every fragment each relay forwarded. If one node hears packets 1, 3, 5, 7 and another hears packets 2, 4, 6, 8, the combined stream decodes cleanly even though neither node alone would have enough data.

**Same key everywhere.** All three roles load the same `/etc/ados/wfb.key`. The shared ChaCha20 session key is derived once; pairing of relays to receiver is a separate concern handled over the batman-adv mesh.

**No drone-side change.** The drone always runs a single `wfb_tx`. Distributed receive is purely a ground-side concern.

## Troubleshooting

| Symptom | Likely Cause | Fix |
|---------|-------------|-----|
| No video, wfb_rx silent | Wrong channel | Match channel on TX and RX |
| "Unable to decrypt session key" | Key mismatch | Re-pair devices |
| Video freezes periodically | High packet loss | Move to less congested channel, check antenna |
| Very short range (<1km) | TX power too low or antenna disconnected | Check `tx_power` config, verify antenna connector |
| wfb_tx/rx won't start | Adapter not in monitor mode | Check driver installation, verify VID:PID |
| High latency (>100ms) | 40MHz bandwidth + high FEC | Switch to 20MHz, reduce FEC K value |
| Receiver aggregator shows 0 relays | Mesh not up between nodes or pairing missing | See `mesh-networking.md` and `pairing-protocol.md` |
| Relay keeps reconnecting | Receiver moved or was replaced; mDNS record stale | Re-pair through the receiver's Accept window |
