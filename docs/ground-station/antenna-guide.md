# Antenna Selection and Mounting Guide

This guide covers antenna selection for 5.8GHz drone video links using WFB-ng. Choosing the right antenna determines your effective range, signal quality, and multipath rejection.

## Antenna Types for 5.8GHz Video

| Type | Gain | Beamwidth | Range (est.) | Price | Best For |
|------|------|-----------|-------------|-------|----------|
| Omni (rubber duck/whip) | 3-5 dBi | 360° H / ~60° V | 5-15 km | $5-15 | Walk-around, portable, short range |
| High-gain omni | 7-9 dBi | 360° H / ~30° V | 15-25 km | $15-30 | Vehicle mount, medium range without tracking |
| Patch (flat panel) | 10-14 dBi | ~60-90° | 25-35 km | $20-40 | Fixed mount, vehicle roof, directional |
| Yagi-Uda | 14-18 dBi | ~30-40° | 40-55 km | $30-60 | Long-range fixed or tracked |
| Helical (RHCP) | 10-14 dBi | ~40-60° | 25-35 km | $25-50 | Long range with circular polarization, best for drones |
| Cloverleaf / Pagoda | 2-3 dBi | ~270° | 2-5 km | $5-15 | Drone TX antenna (omni, circular) |

**Range estimates** assume line of sight, RTL8812EU at ~29dBm TX power, and clear Fresnel zone. Actual range depends on terrain, interference, antenna height, and atmospheric conditions.

## Polarization

Antennas are either linearly polarized (vertical or horizontal) or circularly polarized (RHCP or LHCP).

### Linear Polarization

Dipoles, whips, yagis, and most patch antennas are linearly polarized. The electric field oscillates in one plane. When the transmitting and receiving antennas are aligned (both vertical, for example), signal transfer is at maximum. But when the drone banks into a turn, its antenna rotates relative to the ground antenna. A 90-degree mismatch causes up to 20dB of signal loss. That's enough to kill your video link during aggressive maneuvers.

Linear antennas are cheaper and simpler. They work fine for fixed-wing aircraft that maintain a constant orientation, or for short-range flights where you have plenty of signal margin to absorb the polarization loss.

### Circular Polarization (RHCP / LHCP)

Helical, cloverleaf, pagoda, and some specially-fed patch antennas radiate in a rotating pattern. The electric field corkscrews through space rather than oscillating in one plane. This means the drone can roll, pitch, and yaw without changing the received signal strength. You pay about 3dB compared to a perfectly aligned linear setup, but you never get the catastrophic 20dB drop from cross-polarization.

Circular polarization also rejects multipath reflections. When a circularly polarized signal bounces off the ground or a building, its rotation direction flips (RHCP becomes LHCP). A RHCP receiving antenna naturally rejects LHCP signals by about 15-25dB, which cleans up your signal in environments with ground bounce or urban reflections.

**For drones, circular polarization (RHCP) is the standard choice.** The drone is constantly changing orientation. RHCP on both air and ground is the safest default.

### Polarization Matching Rules

| Rule | Detail |
|------|--------|
| TX and RX must match | Both RHCP or both LHCP. Mixing RHCP TX with LHCP RX causes ~25dB rejection |
| RHCP is the convention | Most FPV antennas are RHCP. Stick with RHCP unless you have a specific reason |
| Cross-pol rejection is useful | Reflected signals flip polarization, so CP antennas naturally reject multipath |
| Linear + circular = 3dB loss | If you mix linear and circular, you lose 3dB regardless of orientation |

## Diversity Reception

WFB-ng supports multi-adapter diversity natively. You plug in two (or more) WiFi adapters, each with its own antenna, and WFB-ng automatically selects the stronger signal on a per-packet basis. No special configuration beyond listing both interfaces in the WFB-ng config.

### How It Works

Each adapter independently receives WFB-ng packets. The receiver aggregates packets from all adapters and uses whichever copy arrived with the best signal quality. This happens at the FEC block level, so even if one adapter drops half the packets in a block, the other adapter's packets fill in the gaps. The result is significantly better reliability than any single adapter can provide.

### Recommended Diversity Setups

**Basic spatial diversity (two omni antennas):**
- Two RTL8812EU adapters, each with a 5dBi omni
- Antennas spaced 15-20cm apart
- Provides multipath fading resistance
- Good for 5-15 km operations

**Directional + omni combo:**
- Adapter 1: patch or helical antenna (pointed at mission area)
- Adapter 2: omni antenna (catches signal during transitions, overhead passes, and behind the ground station)
- Best general-purpose setup for 15-35 km range
- The omni covers your blind spots while the directional extends your range

**Dual directional (with tracker):**
- Two patch or helical antennas on a tracker mount
- Maximum range, but requires antenna tracking hardware
- For 35-55 km operations

**Setup:** Connect both adapters to the ground station USB ports. List both interfaces in the WFB-ng config. The Pro variant includes two USB ports specifically for diversity reception. No additional software configuration needed.

## Antenna Trackers

For long-range operations (25km+), high-gain directional antennas need to point at the drone. Manual tracking is impractical beyond visual range. Antenna trackers read MAVLink GPS telemetry from the drone and drive servo motors to keep the antenna aimed at the drone's position.

| Tracker | Price | Type | Interface | Notes |
|---------|-------|------|-----------|-------|
| Arkbird AAT | ~$200 | Commercial | MAVLink serial | Popular in FPV community, supports multiple telemetry protocols |
| ImmersionRC EzTracker | ~$150 | Commercial | MAVLink serial | Compact, easy setup, good for smaller antennas |
| YANGDA Shadow | ~$500+ | Commercial | MAVLink/PWM | Professional grade, weatherproof housing, handles heavier antennas |
| DIY servo build | ~$50 | Custom | MAVLink via Arduino | Two servos + Arduino + pan-tilt bracket, many open-source designs available |

### How Tracking Works

The ground station receives MAVLink `GLOBAL_POSITION_INT` messages containing the drone's GPS coordinates and altitude. The tracker knows the ground station's GPS position (either from a connected GPS module or manual entry). It computes the bearing and elevation angle from ground to drone, then drives pan and tilt servos to point the antenna. Update rate is typically 5-10Hz, which is fast enough for any multirotor or fixed-wing trajectory.

The ADOS ground station REST API exposes drone GPS position, which can drive a tracker via serial or USB-serial adapter. Connect the tracker's telemetry input to a USB-serial adapter on the SBC.

## Connector Standards

| Connector | Where Used | Notes |
|-----------|-----------|-------|
| RP-SMA | Most WiFi adapter external antenna ports | Reverse polarity SMA. Pin on the cable side, socket on the device. Most common for consumer WiFi gear |
| SMA | Some antennas, professional RF equipment | Standard SMA. Socket on cable, pin on device. Check before buying, easily confused with RP-SMA |
| U.FL / IPEX | PCB-mount WiFi modules (BL-M8812EU2) | Tiny snap-on connector, limited to ~30 mating cycles. Fragile. Use a pigtail to RP-SMA for external antennas |
| N-type | High-power equipment, long cable runs, commercial trackers | Low loss, weatherproof, threaded coupling. Overkill for most drone ground stations |

**Pigtail adapters** convert between connector types. The most common is U.FL to RP-SMA, needed when using PCB WiFi modules with external antennas. Keep pigtails as short as possible (under 15cm) to minimize signal loss.

**Cable loss matters at 5.8GHz.** RG174 coax loses about 1.5dB per meter. RG58 loses about 1.0dB/m. LMR-400 loses about 0.3dB/m. For ground station use, keep antenna cables under 1 meter, or use LMR-400 for longer runs to the antenna. Every dB lost in cable is a dB you can't get back.

## Mounting Guidelines

### Ground Station Antenna

- **Mount as high as possible.** Every meter of height improves line-of-sight distance. A tripod or telescoping mast at 2-3m above ground makes a meaningful difference.
- **Clear line of sight to the mission area.** Trees, buildings, and vehicles between you and the drone cause 20dB+ loss. Walk around your setup to verify the path is clear.
- **If using directional antennas, aim toward the center of your mission area.** The drone will move, so point where it spends most of its time, not where it launches.
- **Separate from other transmitters.** Keep the ground station antenna at least 30cm from your RC transmitter antenna to avoid desensitization.

### Drone Antenna

- **Mount underneath the frame, pointing down toward the ground station.** Most flight time is spent above the ground station, so downward orientation gives the best geometry.
- **Separate from GPS module by at least 10cm.** The 5.8GHz signal can interfere with GPS L1 reception (1575.42 MHz) through harmonic interaction and receiver desensitization.
- **Keep away from ESCs, power distribution boards, and motor wires.** Switching noise from ESCs creates broadband interference that degrades receiver sensitivity.
- **Carbon fiber frames attenuate RF significantly.** Mount the antenna below the frame, not inside or on top of it. Carbon fiber acts as a partial shield.
- **Cloverleaf or pagoda antennas are the standard choice for air units.** They provide omnidirectional coverage with circular polarization, which is exactly what you want on a drone that's constantly changing orientation.

### Ground Plane Considerations

- Omni antennas perform better with a ground plane (metal plate beneath the antenna, or the SBC's PCB acting as one). Without a ground plane, the radiation pattern skews and efficiency drops.
- Patch antennas have a built-in ground plane (the metal backing). No additional ground plane needed.
- Helical antennas include a ground plane reflector at the base.

## Regulatory Power Limits

TX power combined with antenna gain determines your EIRP (Effective Isotropic Radiated Power). This is what regulators measure and limit.

| Region | Authority | Band | Max EIRP | License Required |
|--------|-----------|------|----------|-----------------|
| India | WPC | 5.825-5.875 GHz | 36 dBm (4W) | License-exempt |
| India | WPC | 5.725-5.825 GHz | 30 dBm (1W) | ETA certificate required |
| Australia | ACMA | 5.725-5.875 GHz | ~25 mW EIRP (~14 dBm) | Class license |
| USA | FCC Part 15 | 5.725-5.850 GHz | 30 dBm (1W) | Unlicensed |
| EU | ETSI | 5.470-5.725 GHz | 23 dBm (200mW) | DFS required |

### EIRP Calculation

```
EIRP (dBm) = TX Power (dBm) + Antenna Gain (dBi) - Cable Loss (dB)
```

**Example:** RTL8812EU at 29dBm + 5dBi omni antenna - 0.5dB cable loss = 33.5 dBm EIRP.

This is within India's license-exempt limit (36 dBm) and FCC Part 15 (30 dBm, with additional allowance for point-to-point), but exceeds Australia's ACMA limit and EU ETSI limits significantly.

| Setup | TX Power | Antenna | Cable Loss | EIRP | India (36 dBm) | USA (30 dBm) | Australia (~14 dBm) | EU (23 dBm) |
|-------|----------|---------|-----------|------|----------------|--------------|---------------------|-------------|
| RTL8812EU + omni 5dBi | 20 dBm | 5 dBi | 1 dB | 24 dBm | OK | OK | NO | NO |
| RTL8812EU max + omni 5dBi | 29 dBm | 5 dBi | 1 dB | 33 dBm | OK | NO | NO | NO |
| RTL8812EU max + patch 14dBi | 29 dBm | 14 dBi | 1 dB | 42 dBm | NO | NO | NO | NO |
| RTL8812EU low + omni 3dBi | 10 dBm | 3 dBi | 0.5 dB | 12.5 dBm | OK | OK | OK | OK |

**The agent software-limits TX power based on configured region.** Set your region in the agent config file, and it caps the RTL8812EU's TX power to stay within local regulations. Default is 20dBm (safe for India and USA). For Australia and EU, the agent reduces TX power further, which reduces range accordingly.

Always check your local regulations before operating. This table is a reference summary, not legal advice. Band allocations and power limits vary by country and change over time.

## Recommended Antenna Setups by Use Case

| Use Case | Air Unit Antenna | Ground Station Antenna | Expected Range |
|----------|-----------------|----------------------|---------------|
| Casual flying, parks | Pagoda 3dBi (RHCP) | Omni 5dBi | 5-10 km |
| Survey and mapping | Pagoda 3dBi (RHCP) | Patch 12dBi (RHCP) | 20-30 km |
| Long range BVLOS | Pagoda 3dBi (RHCP) | Helical 14dBi (RHCP) + tracker | 40-50 km |
| Best all-around (diversity) | Pagoda 3dBi (RHCP) | Omni 5dBi + Patch 12dBi | 5-30 km |
| Max range (diversity + tracker) | Pagoda 3dBi (RHCP) | Dual helical 14dBi + tracker | 50+ km |
