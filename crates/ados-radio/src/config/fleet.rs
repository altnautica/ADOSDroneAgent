//! Fleet identity (`link_id`, slots) and the reserved loopback port spans.

/// Largest drone slot a fleet issues. Slots run 1..=24; slot 0 is the ground
/// station. The ceiling is an airtime budget, not a protocol limit — the
/// `link_id` space holds 256 slots per fleet.
pub const FLEET_MAX_SLOTS: u8 = 24;

/// The ground station's fleet slot. A ground station is not an aircraft: it
/// never carries a drone slot and never emits a swarm beacon.
pub const SLOT_GROUND: u8 = 0;

/// wfb-ng `link_id` for a fleet slot. 16 bits of fleet plus 8 bits of slot
/// fills the 24-bit `link_id` space exactly, so two fleets can operate side by
/// side on one channel with no `channel_id` collision
/// (`channel_id = (link_id << 8) + radio_port`).
pub const fn link_id(fleet_id: u16, slot: u8) -> u32 {
    ((fleet_id as u32) << 8) | slot as u32
}

/// Base loopback egress port for a per-slot ground-station video receiver
/// (`wfb_rx -p 0`). Slot S decodes to `VIDEO_RX_PORT_BASE + S`.
pub const VIDEO_RX_PORT_BASE: u16 = 5900;
/// Base loopback egress port for a per-slot ground-station aux receiver
/// (`wfb_rx -p 2`). Slot S decodes to `AUX_RX_PORT_BASE + S`.
pub const AUX_RX_PORT_BASE: u16 = 5930;
/// Base loopback egress port for a per-slot ground-station control receiver
/// (`wfb_rx -p 1`). Slot S decodes to `CONTROL_RX_PORT_BASE + S`.
pub const CONTROL_RX_PORT_BASE: u16 = 5960;

/// The whole loopback span the per-slot receive instances own, inclusive. An
/// operator-settable aux port inside it would be fed by (or would inject into)
/// a fleet receiver, so the load-time guard rejects it.
///
/// The bases sit above the fixed single-instance plane ports (5599-5603, 5803,
/// 5810) deliberately: the drone's own aux defaults are 5602/5603 and the
/// ground station's uplink ingress ports are 5602/5810, all of which would
/// otherwise land inside a per-slot range.
pub const FLEET_RX_PORT_FIRST: u16 = VIDEO_RX_PORT_BASE + 1;
pub const FLEET_RX_PORT_LAST: u16 = CONTROL_RX_PORT_BASE + FLEET_MAX_SLOTS as u16;

/// True when `port` falls inside the per-slot fleet receiver span.
pub const fn is_fleet_rx_port(port: u16) -> bool {
    port >= FLEET_RX_PORT_FIRST && port <= FLEET_RX_PORT_LAST
}

/// Why a fleet identity is unusable. Each variant is a hard load-time refusal,
/// never a silent fallback: a wrong slot is the FEC-thrash failure mode, which
/// presents as unexplained link loss rather than as a config error.
#[derive(Debug, PartialEq, Eq)]
pub enum FleetIdentityError {
    /// `fleet_id` is the reserved "unprovisioned" value 0.
    UnprovisionedFleet,
    /// `fleet_slot` exceeds [`FLEET_MAX_SLOTS`].
    SlotOutOfRange(u8),
    /// A drone profile carries the ground station's slot 0.
    DroneWithoutSlot,
    /// A ground-station profile carries a drone slot.
    GroundWithDroneSlot(u8),
}

impl std::fmt::Display for FleetIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnprovisionedFleet => {
                write!(f, "video.wfb.fleet_id is 0 (reserved as unprovisioned)")
            }
            Self::SlotOutOfRange(s) => write!(
                f,
                "video.wfb.fleet_slot {s} exceeds the maximum fleet slot {FLEET_MAX_SLOTS}"
            ),
            Self::DroneWithoutSlot => write!(
                f,
                "video.wfb.fleet_slot is 0 on a drone profile; slot 0 is the ground station"
            ),
            Self::GroundWithDroneSlot(s) => write!(
                f,
                "video.wfb.fleet_slot {s} on a ground-station profile; a ground station holds slot 0"
            ),
        }
    }
}

/// Validate a fleet identity for a profile. Pure so it is unit-testable without
/// touching a config file; `None` means the identity is usable.
pub fn fleet_identity_error(
    fleet_id: u16,
    fleet_slot: u8,
    is_ground_station: bool,
) -> Option<FleetIdentityError> {
    if fleet_id == 0 {
        return Some(FleetIdentityError::UnprovisionedFleet);
    }
    if fleet_slot > FLEET_MAX_SLOTS {
        return Some(FleetIdentityError::SlotOutOfRange(fleet_slot));
    }
    match (is_ground_station, fleet_slot) {
        (false, SLOT_GROUND) => Some(FleetIdentityError::DroneWithoutSlot),
        (true, s) if s != SLOT_GROUND => Some(FleetIdentityError::GroundWithDroneSlot(s)),
        _ => None,
    }
}

/// The UDP ports the data, stats, and control planes own on the shared adapter:
/// data ingress 5600, data stats 5601, control tx 5803, control rx 5810. The
/// auxiliary pair must never reuse one of these (it would inject into, or steal
/// frames from, a primary plane), and the aux tx/rx ports must differ from each
/// other. Operators can set the aux ports, so this is enforced at load time.
pub const RESERVED_PLANE_PORTS: [u16; 4] = [5600, 5601, 5803, 5810];

/// Why an aux port configuration is invalid, for a loud load-time log. `None`
/// from [`aux_port_collision`] means the aux ports are safe.
#[derive(Debug, PartialEq, Eq)]
pub enum AuxPortCollision {
    /// The aux tx ingress port reuses a reserved data/control plane port.
    TxReservesPlanePort(u16),
    /// The aux rx re-emit port reuses a reserved data/control plane port.
    RxReservesPlanePort(u16),
    /// The aux tx and rx ports are the same (receive would feed transmit).
    TxRxSame(u16),
    /// The aux tx ingress port sits inside the per-slot fleet receiver span, so
    /// a fleet receiver would inject decoded frames straight into the aux
    /// transmitter.
    TxReservesFleetRxPort(u16),
    /// The aux rx re-emit port sits inside the per-slot fleet receiver span, so
    /// the aux receiver and a fleet receiver would both write to one port.
    RxReservesFleetRxPort(u16),
}

/// Validate the auxiliary UDP ports against the reserved data/control/stats
/// ports, the per-slot fleet receiver span, and against each other. Pure (no
/// I/O) so it is unit-testable; returns the first collision found, or `None`
/// when the aux ports are clear.
pub fn aux_port_collision(aux_tx_port: u16, aux_rx_port: u16) -> Option<AuxPortCollision> {
    if aux_tx_port == aux_rx_port {
        return Some(AuxPortCollision::TxRxSame(aux_tx_port));
    }
    if RESERVED_PLANE_PORTS.contains(&aux_tx_port) {
        return Some(AuxPortCollision::TxReservesPlanePort(aux_tx_port));
    }
    if RESERVED_PLANE_PORTS.contains(&aux_rx_port) {
        return Some(AuxPortCollision::RxReservesPlanePort(aux_rx_port));
    }
    if is_fleet_rx_port(aux_tx_port) {
        return Some(AuxPortCollision::TxReservesFleetRxPort(aux_tx_port));
    }
    if is_fleet_rx_port(aux_rx_port) {
        return Some(AuxPortCollision::RxReservesFleetRxPort(aux_rx_port));
    }
    None
}
