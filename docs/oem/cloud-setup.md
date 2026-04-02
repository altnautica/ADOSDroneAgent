# Cloud Infrastructure Requirements for OEM Partners

This document covers everything an OEM needs to deploy for cloud-connected features (remote telemetry, video streaming, fleet management). Cloud connectivity is optional. The agent works fully offline with direct WiFi connection from a phone or laptop.

---

## Architecture Overview

```
Drone (Agent)                    Cloud                         User
┌──────────┐     4G/WiFi     ┌──────────────┐              ┌──────────┐
│ MAVLink  │────────────────→│ MQTT Broker  │──────────────│ GCS App  │
│ Video    │────────────────→│ TURN Server  │──────────────│ (browser)│
│ Status   │                 │ Video Relay  │              │          │
└──────────┘                 └──────────────┘              └──────────┘
```

Three cloud services are needed. MQTT is required. STUN/TURN is required for video. Video Relay is a fallback.

---

## 1. MQTT Broker

MQTT handles telemetry, status, and commands between drone and GCS.

### Option A: Self-Hosted (recommended for >50 drones)

Deploy Mosquitto or EMQX on any VPS.

**Minimum requirements:**
- 1 vCPU, 1GB RAM, 10GB disk
- Cost: $5-10/month (DigitalOcean, Vultr, Hetzner)
- Scales to ~500 concurrent connections on this spec

**Mosquitto config (`mosquitto.conf`):**

```conf
# MQTT over TCP
listener 1883
protocol mqtt

# MQTT over WebSocket (required for browser GCS)
listener 9001
protocol websockets

# Authentication
allow_anonymous false
password_file /mosquitto/config/passwords

# Logging
log_dest stdout
log_type all

# Persistence
persistence true
persistence_location /mosquitto/data/
```

**Create password file:**

```bash
# Create password file with a device account
mosquitto_passwd -c /mosquitto/config/passwords ados-device-001

# Add more devices
mosquitto_passwd /mosquitto/config/passwords ados-device-002
```

**Docker Compose:**

```yaml
services:
  mosquitto:
    image: eclipse-mosquitto:2
    ports:
      - "1883:1883"
      - "9001:9001"
    volumes:
      - ./mosquitto.conf:/mosquitto/config/mosquitto.conf
      - ./passwords:/mosquitto/config/passwords
      - mosquitto_data:/mosquitto/data

volumes:
  mosquitto_data:
```

### Option B: Altnautica-Hosted

Use `mqtt.altnautica.com` (WebSocket on port 443 via Cloudflare Tunnel).

- Included in Pro tier ($29/mo) and Enterprise tier ($99+/mo)
- No deployment needed on your side
- We handle uptime, scaling, and monitoring
- Device credentials provisioned via API

### Topic Structure

```
ados/{deviceId}/status       # Device status (heartbeat, GPS, battery, mode)
ados/{deviceId}/telemetry    # High-rate telemetry (attitude, RC channels, sensors)
ados/{deviceId}/commands     # Commands from GCS to device
ados/{deviceId}/commands/ack # Command acknowledgments from device
ados/{deviceId}/video/offer  # WebRTC signaling (SDP offer)
ados/{deviceId}/video/answer # WebRTC signaling (SDP answer)
ados/{deviceId}/video/ice    # WebRTC ICE candidates
```

All topics use QoS 1 (at least once delivery). Retained messages enabled for `status` topic only.

---

## 2. STUN Server

STUN (Session Traversal Utilities for NAT) helps establish direct peer-to-peer video connections by discovering the device's public IP and port.

**No deployment needed.** Use free public STUN servers:

```
stun:stun.l.google.com:19302
stun:stun1.l.google.com:19302
stun:stun.cloudflare.com:3478
stun:stun.stunprotocol.org:3478
```

The agent configuration accepts a list of STUN servers:

```yaml
# /etc/ados/config.yaml
webrtc:
  stun_servers:
    - "stun:stun.l.google.com:19302"
    - "stun:stun.cloudflare.com:3478"
```

STUN works for ~70-80% of network configurations. For the remaining cases (carrier-grade NAT on 4G networks), you need TURN.

---

## 3. TURN Server (Recommended: eturnal)

When the drone agent is on 4G cellular, carrier-grade NAT often blocks peer-to-peer WebRTC connections. A TURN server relays video traffic in these cases. About 20-40% of 4G connections need TURN.

### eturnal (recommended for OEM deployments)

eturnal is a lightweight Erlang-based TURN server. 70% lighter than coturn (20-30MB vs 50-100MB+), with YAML configuration instead of coturn's complex .conf format.

Minimum VPS: 1 vCPU, 512MB RAM, high bandwidth allocation.

```yaml
# /etc/eturnal.yml
eturnal:
  secret: "generate-a-strong-secret-here"
  relay_min_port: 49152
  relay_max_port: 65535
  listen:
    - ip: "::"
      port: 3478
      transport: udp
    - ip: "::"
      port: 3478
      transport: tcp
  modules:
    mod_log_stun:
      level: notice
```

```bash
# Docker deployment (simplest)
docker run -d --name eturnal \
  --network host \
  -v /etc/eturnal.yml:/etc/eturnal.yml \
  ghcr.io/processone/eturnal

# Or apt (Debian/Ubuntu)
apt install eturnal
systemctl enable --now eturnal
```

Test with: `turnutils_uclient -T -u test -w test your-server-ip`

### coturn (alternative, heavier)

coturn is the industry standard TURN server written in C. Heavier to configure and run, but more battle-tested at high scale (200+ concurrent connections).

Use coturn if you already have experience with it or need to handle 200+ concurrent drone video streams.

### Cloudflare TURN (managed, zero ops)

Cloudflare offers a managed TURN service. No server management. Pay per usage. Good for OEMs who don't want to manage infrastructure. See developers.cloudflare.com/realtime/turn/.

### Bandwidth Planning

| Video Quality | Bitrate | Per Hour Through TURN | Per 8hr Day |
|--------------|---------|----------------------|-------------|
| 720p@25 H.264 | 2 Mbps | ~0.9 GB | ~7.2 GB |
| 1080p@30 H.264 | 4 Mbps | ~1.8 GB | ~14.4 GB |
| 1080p@30 H.265 | 2 Mbps | ~0.9 GB | ~7.2 GB |

VPS bandwidth: $0.01-0.05/GB. One drone at 720p through TURN for 4 hours/day costs ~$0.04-0.18/day.

Recommendation: Use 720p@2Mbps over 4G to minimize TURN bandwidth. H.265 halves the cost if the chip supports it.

---

## 4. Video Relay (Fallback)

If WebRTC via STUN/TURN fails (rare, but possible with very restrictive firewalls), the agent falls back to streaming via a video relay server.

The relay converts RTSP from the agent to fMP4-over-WebSocket, which any browser can play natively using MediaSource Extensions.

**Docker image available at:** `ADOSMissionControl/tools/video-relay/`

```yaml
services:
  video-relay:
    build: ./video-relay
    ports:
      - "3001:3001"
    environment:
      - PORT=3001
    restart: unless-stopped
```

The relay spawns an ffmpeg process per active video stream (copy codec, zero transcoding). Each stream uses ~50MB RAM and minimal CPU.

**When to deploy this:** Only if you get reports from customers that video doesn't work. WebRTC with STUN+TURN covers 95%+ of cases.

---

## 5. Domain and SSL

### What You Need

- A domain name (e.g., `cloud.hglrc-drones.com`)
- SSL certificate (free via Let's Encrypt or Cloudflare)
- DNS records for subdomains

### Recommended Subdomains

| Subdomain | Service | Port |
|-----------|---------|------|
| `mqtt.yourdomain.com` | MQTT broker (WebSocket) | 9001 |
| `turn.yourdomain.com` | TURN server | 3478/5349 |
| `video.yourdomain.com` | Video relay (fallback) | 3001 |

### Cloudflare Tunnel (Recommended)

Cloudflare Tunnel eliminates the need to open inbound ports on your VPS. Free tier is sufficient.

```bash
# Install cloudflared on your VPS
curl -L https://pkg.cloudflare.com/cloudflared-stable-linux-amd64.deb -o cloudflared.deb
sudo dpkg -i cloudflared.deb

# Authenticate
cloudflared tunnel login

# Create tunnel
cloudflared tunnel create ados-cloud

# Configure routes
cat > ~/.cloudflared/config.yml << 'CONFIG'
tunnel: YOUR_TUNNEL_ID
credentials-file: /root/.cloudflared/YOUR_TUNNEL_ID.json

ingress:
  - hostname: mqtt.yourdomain.com
    service: http://localhost:9001
  - hostname: video.yourdomain.com
    service: http://localhost:3001
  - service: http_status:404
CONFIG

# Run
cloudflared tunnel run ados-cloud
```

Add CNAME records in Cloudflare DNS pointing each subdomain to `YOUR_TUNNEL_ID.cfargotunnel.com`.

---

## 6. Cost Estimates

All prices in USD. Based on commodity VPS pricing (DigitalOcean, Vultr, Hetzner).

### Small Fleet (up to 50 drones)

| Item | Monthly Cost |
|------|-------------|
| VPS (2 vCPU, 2GB, Mosquitto + coturn) | $10-15 |
| Bandwidth (~500GB/mo) | $5-10 |
| Domain + SSL | $0 (Cloudflare free) |
| **Total** | **$15-25/mo** |

### Medium Fleet (50-500 drones)

| Item | Monthly Cost |
|------|-------------|
| VPS 1: MQTT broker (2 vCPU, 4GB) | $20 |
| VPS 2: TURN server (4 vCPU, 4GB) | $30-40 |
| Bandwidth (~5TB/mo) | $50-100 |
| Monitoring (Grafana Cloud free tier) | $0 |
| **Total** | **$100-160/mo** |

### Large Fleet (500-5000 drones)

| Item | Monthly Cost |
|------|-------------|
| MQTT cluster (3 nodes, EMQX) | $60-100 |
| TURN servers (2-3 regions) | $60-120 |
| Bandwidth (~20-50TB/mo) | $200-500 |
| Monitoring + alerting | $20-50 |
| **Total** | **$340-770/mo** |

At large scale, consider Altnautica's Enterprise tier ($99+/mo) which includes all cloud infrastructure, multi-region TURN, and monitoring. Break-even is around 200-300 active drones.
