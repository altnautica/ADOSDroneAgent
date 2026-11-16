//! Whether the vehicle flies with an RC transmitter, as the operator declared
//! it in setup.

use serde::{Deserialize, Serialize};

/// The operator's declaration of an RC receiver on the vehicle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RcReceiver {
    /// An RC transmitter flies with the vehicle: the flight controller's own
    /// RC failsafe guards its loss.
    #[default]
    Present,
    /// No RC transmitter (controller-free flight from an app): the flight
    /// controller's RC failsafe is off, its fence is on, and the pilot's
    /// link-loss watch covers every airborne owner and every operator
    /// instance on its own.
    Absent,
}

impl RcReceiver {
    pub fn is_present(&self) -> bool {
        *self == Self::Present
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
        }
    }
}
