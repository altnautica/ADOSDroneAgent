//! `ados-clock`: the host clock service and the sensor-bus broker.
//!
//! It binds `/run/ados/sensing/sensors.sock` and brokers it for every producer
//! and consumer. From the broker's tap it fits each time source that publishes
//! `sync` pairs (flight controller, sensor hub, GNSS PPS, simulator) against
//! the host monotonic clock (the arrival-stamped simulator pairs by their
//! lower envelope), picks the timebase the next capture session uses,
//! and tracks the rate and freshness of every data stream. Once a second it
//! publishes a `clock` frame and writes `/run/ados/sensing/clock.json` and
//! `/run/ados/sensors.json`. The `sensing` directory holds only the socket
//! and the clock sidecar, so a plugin granted `sensor.stream.read` gets it
//! bound whole and follows every rename and socket re-creation. A locked GNSS
//! PPS source is also handed to chrony through its `SOCK` refclock.

pub mod bus;
pub mod chrony;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod engine;
pub mod envelope;
pub mod fit;
pub mod streams;

pub use bus::spawn_bus;
pub use engine::Engine;
