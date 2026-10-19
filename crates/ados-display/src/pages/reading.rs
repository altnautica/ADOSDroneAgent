//! The tri-state of an optional agent route as the pages read it.
//!
//! A route the agent answers with `present: false` (or does not serve for this
//! profile) is [`Reading::Absent`]: the hardware is not fitted. A request that
//! failed in transport, timed out or returned an error status is
//! [`Reading::Unreachable`]: the state is unknown, not absent. The two render
//! differently ("not fitted" vs "unreachable") so a transient fetch failure is
//! never shown as missing hardware.

/// One optional route's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Reading<T> {
    /// Not fitted, not polled for this profile, or answered `present: false`.
    #[default]
    Absent,
    /// The request failed; the state is unknown.
    Unreachable,
    /// The route answered with a fitted service.
    Present(T),
}

impl<T> Reading<T> {
    /// The value when present.
    pub fn present(&self) -> Option<&T> {
        match self {
            Self::Present(v) => Some(v),
            _ => None,
        }
    }

    /// The value when present, mutably.
    pub fn present_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Present(v) => Some(v),
            _ => None,
        }
    }

    /// Whether the route answered with a fitted service.
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    /// Whether the route said the service is not fitted (or was not read).
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// The value when present, by value.
    pub fn into_present(self) -> Option<T> {
        match self {
            Self::Present(v) => Some(v),
            _ => None,
        }
    }

    /// Map the present value, keeping the absent / unreachable state.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Reading<U> {
        match self {
            Self::Present(v) => Reading::Present(f(v)),
            Self::Absent => Reading::Absent,
            Self::Unreachable => Reading::Unreachable,
        }
    }

    /// Map the present value through a fallible mapper: a `None` result (the
    /// body said `present: false` or did not decode as fitted) is absent.
    pub fn and_then<U>(self, f: impl FnOnce(T) -> Option<U>) -> Reading<U> {
        match self {
            Self::Present(v) => f(v).map_or(Reading::Absent, Reading::Present),
            Self::Absent => Reading::Absent,
            Self::Unreachable => Reading::Unreachable,
        }
    }

    /// The panel body text for a non-present reading of `what` (e.g.
    /// "GNSS receiver"): "No GNSS receiver fitted" or "GNSS receiver
    /// unreachable". `None` when present.
    pub fn missing_text(&self, what: &str) -> Option<String> {
        match self {
            Self::Present(_) => None,
            Self::Absent => Some(format!("No {what} fitted")),
            Self::Unreachable => Some(format!("{what} unreachable")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_unreachable_read_differently() {
        let a: Reading<u8> = Reading::Absent;
        let u: Reading<u8> = Reading::Unreachable;
        assert_eq!(
            a.missing_text("LoRa module").as_deref(),
            Some("No LoRa module fitted")
        );
        assert_eq!(
            u.missing_text("LoRa module").as_deref(),
            Some("LoRa module unreachable")
        );
        assert_eq!(Reading::Present(1u8).missing_text("x"), None);
    }

    #[test]
    fn mapping_keeps_the_state() {
        assert_eq!(Reading::Present(2).map(|v| v * 2), Reading::Present(4));
        assert_eq!(Reading::<u8>::Unreachable.map(|v| v), Reading::Unreachable);
        assert_eq!(
            Reading::Present(2).and_then(|_| None::<u8>),
            Reading::Absent
        );
        assert_eq!(
            Reading::<u8>::Unreachable.and_then(Some),
            Reading::Unreachable
        );
        assert!(Reading::Present(0).is_present());
        assert!(!Reading::<u8>::Unreachable.is_present());
    }
}
