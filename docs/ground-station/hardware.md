# ADOS Ground Station — Hardware Variants

## Product Tiers

| Variant | Board | Adapter(s) | Display | Key Feature | Target Price |
|---------|-------|-----------|---------|-------------|-------------|
| USB Dongle | RTL8812EU only | Self | None | Plug into Linux laptop | $30-50 |
| Lite | Radxa CM3 (RK3566, 2GB) | 1x RTL8812EU | WiFi AP to browser | Turnkey, cheapest | $100-150 |
| Pro | Radxa CM4 (RK3588S2, 4GB) | 2x RTL8812EU | HDMI + WiFi AP | Diversity + local display | $200-350 |
| Field Kit | Raspberry Pi 5 (4GB) + case | 1-2x RTL8812EU | 7" HDMI touchscreen | Complete portable station | $250-400 |

## Baseboard Reuse

Same reference baseboard PCB works for both air unit and ground station. The only difference is software mode (TX vs RX) and which peripherals are connected.

| Peripheral | Air Unit | Ground Station |
|-----------|----------|---------------|
| Camera (CSI) | Connected | Not used |
| FC (UART) | Connected | Not used |
| RTL8812EU (USB) | Connected (TX) | Connected (RX) |
| HDMI output | Not used | Optional (Pro variant) |
| 4G modem (USB) | Optional (BVLOS telemetry) | Optional (cloud uplink) |
| WiFi AP | Optional | Always active |

One hardware design. Two products. Differentiated entirely by software config.

## RTL8812EU Adapters

| Adapter | Chipset | Form Factor | TX Power | Price | Notes |
|---------|---------|-------------|----------|-------|-------|
| LB-LINK BL-M8812EU2 | RTL8812EU | 30x30mm module | 29dBm (800mW) | ~$10 | Best for baseboard integration |
| ALFA AWUS036ACH | RTL8812AU | USB-A dongle, dual antenna | 20dBm (100mW) | ~$52 | External, dual RP-SMA |
| Generic USB-C dongle | RTL8812EU | USB-C stick | 29dBm | $15-25 | Consumer-friendly |

VID:PID for auto-detection: RTL8812EU = `0BDA:B812`

**WARNING:** RTL8812BU (different chip) does NOT support monitor mode. Do not use for WFB-ng. The chip names look similar but they are fundamentally different silicon.

## Display Options (Pro and Field Kit Variants)

| Display | Size | Resolution | Brightness | Price | Notes |
|---------|------|-----------|------------|-------|-------|
| Generic HDMI (Pi-compatible) | 7" | 1024x600 | 300-500 nit | $40-80 | Indoor/shade only |
| High-brightness IPS | 7" | 1024x600 | 1000+ nit | $200-400 | Outdoor direct sunlight |
| Walksnail Avatar Goggles X | FPV goggles | 1080p | N/A | $400-600 | Immersive, HDMI input |
| Fatshark + Avatar VRX | FPV goggles | varies | N/A | $300-550 | Legacy goggles + HDMI receiver |

## Power Consumption

| Variant | Idle | Active (video RX) | Power Source |
|---------|------|-------------------|-------------|
| Lite (CM3) | ~2W | ~5W | USB-C 5V/2A |
| Pro (CM4) | ~3W | ~8W | USB-C 5V/3A |
| Field Kit (Pi 5 + screen) | ~5W | ~12-15W | USB-C PD 27W adapter or LiPo battery |

The Lite variant runs on any standard phone charger. The Pro variant needs a slightly beefier USB-C supply. The Field Kit benefits from a dedicated battery pack for untethered field use.

## Per-Role BOM Deltas

Single-node `direct` is the default. `relay` and `receiver` are opt-in when a deployment spans obstructed terrain or long corridors.

| Role | Role purpose | Extra hardware over `direct` | Why |
|------|---|---|---|
| `direct` | One node serves the pilot end-to-end | None | Baseline. 1× RTL8812EU (WFB-ng RX), 1× antenna, the SBC itself |
| `relay` | Forwards WFB-ng fragments it heard to the receiver | + 1× generic USB WiFi dongle (mesh carrier), + 1× small antenna for that dongle | batman-adv mesh runs on a dedicated interface so it does not compete with WFB-ng for airtime on the primary adapter |
| `receiver` | Hub. Combines fragments from relays into one clean stream | + 1× generic USB WiFi dongle (mesh carrier), + 1× small antenna for that dongle | Same reason as relay. The receiver also publishes the mesh service record on `bat0` |

**Mesh carrier adapter requirements.**

- Linux driver that supports 802.11s or IBSS mode (verified with `iw list`)
- USB 2.0 port on the host SBC is enough. A USB 3.0 port is fine.
- Any 2.4 GHz or 5 GHz chipset works. Typical price range USD 8-15.
- Do NOT reuse the RTL8812EU for mesh. The WFB-ng primary adapter stays dedicated to WFB-ng RX.

**Cloud uplink stays optional per node.** Any node (relay or receiver) with a WiFi client, Ethernet, or 4G connection can advertise itself as the mesh cloud gateway. No extra hardware is required for gateway election; the existing 4G modem or Ethernet port does the work.
