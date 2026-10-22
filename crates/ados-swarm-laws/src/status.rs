//! The beacon `status` byte: five condition bits and the three-bit active
//! precedence field.
//!
//! Every control law reads a neighbour's flags through these constants (see
//! [`crate::NeighborState`]), and a transport that encodes the beacon uses the
//! same ones, so the bit positions exist in exactly one place.

/// `status` bit 0: the flight controller reports the vehicle armed.
pub const STATUS_ARMED: u8 = 1 << 0;
/// `status` bit 1: the vehicle is in a mode that accepts offboard setpoints
/// (GUIDED). The control layer only commands a vehicle that reports this.
pub const STATUS_GUIDED: u8 = 1 << 1;
/// `status` bit 2: the separation layer has taken over (a neighbour inside the
/// hard-separation radius).
pub const STATUS_EMERGENCY: u8 = 1 << 2;
/// `status` bit 3: the GPS fix is usable (3D or better). A beacon with this bit
/// clear carries a position no other drone may separate against.
pub const STATUS_GPS_OK: u8 = 1 << 3;
/// `status` bit 4: this drone is the operator-selected hero (for example, the
/// one streaming full video). Exclusive fleet-wide.
pub const STATUS_HERO: u8 = 1 << 4;

/// `status` bits 5-7: the sender's currently active
/// [`ModePrecedence`](crate::ModePrecedence). Three bits hold all five levels
/// with room to spare, so the per-neighbour precedence truth costs zero extra
/// bytes.
///
/// [`ModePrecedence::Hold`](crate::ModePrecedence::Hold) is discriminant 0, so a
/// node with no autonomy layer running radiates `000` here and every reader
/// decodes `hold`.
pub const STATUS_PRECEDENCE_MASK: u8 = 0b1110_0000;

/// How far to shift a [`ModePrecedence`](crate::ModePrecedence) discriminant
/// into `status`.
pub const STATUS_PRECEDENCE_SHIFT: u32 = 5;
