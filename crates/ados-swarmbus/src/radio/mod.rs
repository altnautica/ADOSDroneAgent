//! The platform seam: raw 802.11 injection and capture.
//!
//! One type, two implementations, selected at compile time. The Linux one is four
//! syscalls over `AF_PACKET`; the other refuses to open. Everything above this seam
//! is platform-independent, which is what lets the whole wire format, cipher,
//! neighbour table and scheduling be developed and tested on a macOS host.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod stub;

#[cfg(target_os = "linux")]
pub use linux::Radio;
#[cfg(not(target_os = "linux"))]
pub use stub::Radio;
