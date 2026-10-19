//! The bus frame and its payload structs.

use serde::{Deserialize, Serialize};

use crate::{MAX_FRAME_BYTES, SENSOR_BUS_VERSION};

/// What a frame carries; selects the payload encoding (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Clock,
    Imu,
    Gnss,
    Trigger,
    Baro,
    Mag,
    Rtcm,
    Sync,
    Subscribe,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Clock => "clock",
            Kind::Imu => "imu",
            Kind::Gnss => "gnss",
            Kind::Trigger => "trigger",
            Kind::Baro => "baro",
            Kind::Mag => "mag",
            Kind::Rtcm => "rtcm",
            Kind::Sync => "sync",
            Kind::Subscribe => "subscribe",
        }
    }
}

/// Where a stream's samples come from; `/api/v1/sensors` reports it as the
/// sensor `source` and picks the clock source whose sigma applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The sensor hub (`ados-sensorhub`).
    Hub,
    /// The flight controller over MAVLink (`ados-mavlink-router`).
    Fc,
    /// A camera owned by the vision engine.
    Vision,
    /// A GNSS receiver read directly by `ados-gnss`.
    Gnss,
    /// A LiDAR driver.
    Lidar,
    /// A simulator-fed source.
    Sim,
    /// The host itself (the REST surface, the clock).
    Host,
}

impl Origin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Origin::Hub => "hub",
            Origin::Fc => "fc",
            Origin::Vision => "vision",
            Origin::Gnss => "gnss",
            Origin::Lidar => "lidar",
            Origin::Sim => "sim",
            Origin::Host => "host",
        }
    }
}

/// One sensor-bus frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusFrame {
    /// [`SENSOR_BUS_VERSION`]; a frame with any other value is rejected.
    pub v: u16,
    pub kind: Kind,
    /// Stream id: the rig sensor id the samples belong to (`imu0`, `gnss0`,
    /// `cam_fl`, ...), `^[a-z0-9_]+$`, so it can name an MCAP channel.
    pub src: String,
    pub origin: Origin,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

impl BusFrame {
    pub fn new(kind: Kind, src: impl Into<String>, origin: Origin, payload: Vec<u8>) -> Self {
        BusFrame {
            v: SENSOR_BUS_VERSION,
            kind,
            src: src.into(),
            origin,
            payload,
        }
    }

    /// A frame whose payload is a prost message.
    pub fn proto<M: prost::Message>(
        kind: Kind,
        src: impl Into<String>,
        origin: Origin,
        msg: &M,
    ) -> Self {
        Self::new(kind, src, origin, msg.encode_to_vec())
    }

    /// A frame whose payload is a msgpack map.
    pub fn msgpack<T: Serialize>(
        kind: Kind,
        src: impl Into<String>,
        origin: Origin,
        body: &T,
    ) -> Result<Self, FrameError> {
        Ok(Self::new(kind, src, origin, rmp_serde::to_vec_named(body)?))
    }

    /// Decode the payload as a prost message.
    pub fn decode_proto<M: prost::Message + Default>(&self) -> Result<M, FrameError> {
        Ok(M::decode(self.payload.as_slice())?)
    }

    /// Decode the payload as a msgpack map.
    pub fn decode_msgpack<T: serde::de::DeserializeOwned>(&self) -> Result<T, FrameError> {
        Ok(rmp_serde::from_slice(&self.payload)?)
    }
}

/// Errors encoding or decoding a frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("msgpack encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("protobuf decode: {0}")]
    Proto(#[from] prost::DecodeError),
    #[error("unsupported sensor bus version {0}")]
    Version(u16),
    #[error("frame of {0} bytes exceeds the bus limit")]
    TooLarge(usize),
}

/// Encode a frame as `len:u32be | msgpack`.
pub fn encode(frame: &BusFrame) -> Result<Vec<u8>, FrameError> {
    let body = rmp_serde::to_vec_named(frame)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one frame body (the bytes after the length prefix).
pub fn decode_body(body: &[u8]) -> Result<BusFrame, FrameError> {
    let frame: BusFrame = rmp_serde::from_slice(body)?;
    if frame.v != SENSOR_BUS_VERSION {
        return Err(FrameError::Version(frame.v));
    }
    Ok(frame)
}

/// Barometer sample (`kind: baro`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Baro {
    pub t_ns: u64,
    pub pressure_pa: f64,
    pub temp_c: f32,
}

/// Magnetometer sample in body FLU, microtesla (`kind: mag`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Mag {
    pub t_ns: u64,
    pub field_ut: [f32; 3],
}

/// The time source a [`SyncObs`] relates to the host clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncSource {
    /// Flight-controller boot time (TIMESYNC).
    Fc,
    /// Sensor-hub timebase.
    Hub,
    /// GNSS PPS: `source_ns` is UTC (Unix epoch) ns of the pulse.
    GnssPps,
    /// Simulator time (`sim_time_ns` from the renderer SEI or the sim hub).
    Sim,
}

impl SyncSource {
    pub const ALL: [SyncSource; 4] = [
        SyncSource::Fc,
        SyncSource::Hub,
        SyncSource::GnssPps,
        SyncSource::Sim,
    ];

    /// The `sources[].id` in `/api/v1/clock`.
    pub const fn id(self) -> &'static str {
        match self {
            SyncSource::Fc => "fc",
            SyncSource::Hub => "hub",
            SyncSource::GnssPps => "gnss",
            SyncSource::Sim => "sim",
        }
    }

    /// The `sources[].kind` in `/api/v1/clock`.
    pub const fn kind(self) -> &'static str {
        match self {
            SyncSource::Fc => "fc",
            SyncSource::Hub => "hub",
            SyncSource::GnssPps => "gnss_pps",
            SyncSource::Sim => "sim",
        }
    }
}

/// One host↔source time pair (`kind: sync`): at host monotonic `host_ns` the
/// source clock read `source_ns`. `rtt_ns` is the exchange round trip when
/// the pair comes from a two-way exchange (TIMESYNC), else 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncObs {
    pub source: SyncSource,
    pub host_ns: u64,
    pub source_ns: i64,
    pub rtt_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1;

    #[test]
    fn frame_round_trips_with_proto_payload() {
        let batch = v1::ImuBatch {
            samples: vec![v1::ImuSample {
                t_ns: 5,
                seq: 1,
                gyro: vec![0.1, 0.2, 0.3],
                accel: vec![0.0, 0.0, 9.81],
                temp_c: Some(31.5),
            }],
        };
        let f = BusFrame::proto(Kind::Imu, "imu0", Origin::Hub, &batch);
        let wire = encode(&f).unwrap();
        let len = u32::from_be_bytes(wire[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, wire.len() - 4);
        let back = decode_body(&wire[4..]).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.decode_proto::<v1::ImuBatch>().unwrap(), batch);
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut f = BusFrame::new(Kind::Rtcm, "rest", Origin::Host, vec![1, 2, 3]);
        f.v = 9;
        let wire = encode(&f).unwrap();
        assert!(matches!(
            decode_body(&wire[4..]),
            Err(FrameError::Version(9))
        ));
    }

    #[test]
    fn kinds_serialize_snake_case() {
        let f = BusFrame::msgpack(
            Kind::Sync,
            "router",
            Origin::Fc,
            &SyncObs {
                source: SyncSource::GnssPps,
                host_ns: 1,
                source_ns: -2,
                rtt_ns: 0,
            },
        )
        .unwrap();
        #[derive(Deserialize)]
        struct KindOnly {
            kind: String,
        }
        let k: KindOnly = rmp_serde::from_slice(&rmp_serde::to_vec_named(&f).unwrap()).unwrap();
        assert_eq!(k.kind, "sync");
        let obs: SyncObs = f.decode_msgpack().unwrap();
        assert_eq!(obs.source, SyncSource::GnssPps);
    }
}
