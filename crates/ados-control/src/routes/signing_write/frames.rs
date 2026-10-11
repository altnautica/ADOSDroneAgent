//! MAVLink frame checks, key parsing and SETUP_SIGNING frame building.

use super::*;

/// The MAVLink frame layout the signing checks read.
pub(super) const V2_MAGIC: u8 = 0xFD;
pub(super) const V1_MAGIC: u8 = 0xFE;
pub(super) const V2_HEADER_LEN: usize = 10;
pub(super) const V2_CRC_LEN: usize = 2;
pub(super) const V2_SIGNATURE_BLOCK_LEN: usize = 13;
pub(super) const V2_SIGNATURE_LEN: usize = 6;
pub(super) const IFLAG_SIGNED: u8 = 0x01;

/// Whether `frame` is a MAVLink 2 frame from `system` carrying a valid
/// signature under `key`. The signature is the first 6 bytes of
/// sha256(key ‖ every frame byte before the signature), which covers the
/// header, payload, checksum, link id and timestamp.
pub(super) fn signed_by(frame: &[u8], system: u8, key: &[u8]) -> bool {
    if frame.len() < V2_HEADER_LEN
        || frame[0] != V2_MAGIC
        || frame[2] & IFLAG_SIGNED == 0
        || frame[5] != system
    {
        return false;
    }
    let total = V2_HEADER_LEN + usize::from(frame[1]) + V2_CRC_LEN + V2_SIGNATURE_BLOCK_LEN;
    if frame.len() != total {
        return false;
    }
    let (signed, signature) = frame.split_at(total - V2_SIGNATURE_LEN);
    let digest = Sha256::new()
        .chain_update(key)
        .chain_update(signed)
        .finalize();
    digest[..V2_SIGNATURE_LEN] == *signature
}

/// Whether `frame` is an unsigned frame from `system`: a MAVLink 2 frame with
/// the signed flag clear, or any MAVLink 1 frame (v1 has no signing).
pub(super) fn unsigned_from(frame: &[u8], system: u8) -> bool {
    match frame.first() {
        Some(&V2_MAGIC) => {
            frame.len() >= V2_HEADER_LEN && frame[2] & IFLAG_SIGNED == 0 && frame[5] == system
        }
        Some(&V1_MAGIC) => frame.len() >= 6 && frame[3] == system,
        _ => false,
    }
}

/// The disable target: the body's system/component (bounded like enrolment),
/// each falling back to the resolved FC target `fc` when omitted.
pub(super) fn disable_target(
    body: Option<DisableRequest>,
    fc: (u8, u8),
) -> Result<(u8, u8), &'static str> {
    let Some(b) = body else {
        return Ok(fc);
    };
    let system = bounded_u8(b.target_system.unwrap_or(i64::from(fc.0)), 1, 255)
        .map_err(|()| "target_system out of range")?;
    let component = bounded_u8(b.target_component.unwrap_or(i64::from(fc.1)), 0, 255)
        .map_err(|()| "target_component out of range")?;
    Ok((system, component))
}

/// Parse a 64-char lowercase-hex string into a 32-byte key, mirroring the Python
/// `parse_key_hex` error messages verbatim so the 400 body is byte-identical:
/// a non-64-length input is `key_hex must be 64 hex chars, got <n>`; a non-hex
/// input is `key_hex is not valid hex: <reason>`.
pub(super) fn parse_key_hex(key_hex: &str) -> Result<Vec<u8>, String> {
    if key_hex.len() != 64 {
        return Err(format!(
            "key_hex must be 64 hex chars, got {}",
            key_hex.len()
        ));
    }
    decode_hex(key_hex).map_err(|reason| format!("key_hex is not valid hex: {reason}"))
}

/// Decode a hex string (even length, only `0-9a-fA-F`) into bytes. Returns a
/// short reason string on a bad char or an odd length, used to build the Python
/// `key_hex is not valid hex:` message. (The length is already checked to 64 by
/// the caller, so the odd-length arm is defensive.)
pub(super) fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length string".to_string());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// One hex nibble (0-15) for an ASCII hex digit, or a reason string for a
/// non-hex byte.
pub(super) fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(format!("Non-hexadecimal digit found: '{}'", other as char)),
    }
}

/// Coerce a JSON-parsed integer into a `u8` within `[lo, hi]`, returning `Err`
/// when out of range — the Rust equivalent of the Pydantic `ge`/`le` field
/// validators. An in-range value is the byte; anything outside is a 400 at the
/// call site.
pub(super) fn bounded_u8(value: i64, lo: i64, hi: i64) -> Result<u8, ()> {
    if (lo..=hi).contains(&value) {
        Ok(value as u8)
    } else {
        Err(())
    }
}

/// The first 8 hex chars of sha256 over the key — a fingerprint safe to display
/// and log, mirroring the Python `_fingerprint`.
pub(super) fn fingerprint(key: &[u8]) -> String {
    let digest = Sha256::digest(key);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..8].to_string()
}

/// Seconds since 2015-01-01 UTC, in 10-microsecond units, mirroring the Python
/// `_initial_timestamp_10us` (`int((time.time() - EPOCH_2015) * 100_000)`).
pub(super) fn initial_timestamp_10us(now: OffsetDateTime) -> u64 {
    let seconds_since_2015 = now.unix_timestamp() as f64 + now.nanosecond() as f64 / 1e9
        - EPOCH_2015_UNIX_SECONDS as f64;
    let scaled = (seconds_since_2015 * TEN_US_PER_SECOND) as i64;
    scaled.max(0) as u64
}

/// Build a `SETUP_SIGNING` v2 frame with the signing source identity. The key is
/// copied into the 32-byte field (callers pass exactly 32 bytes). Returns `Err`
/// only on a serialize failure, which the route maps to a 500.
pub(super) fn build_setup_signing_frame(
    target_system: u8,
    target_component: u8,
    key: &[u8],
    initial_timestamp: u64,
) -> Result<Vec<u8>, ()> {
    let mut secret_key = [0u8; SIGNING_KEY_LEN];
    let n = key.len().min(SIGNING_KEY_LEN);
    secret_key[..n].copy_from_slice(&key[..n]);
    let msg = MavMessage::SETUP_SIGNING(SETUP_SIGNING_DATA {
        target_system,
        target_component,
        secret_key,
        initial_timestamp,
    });
    serialize_signing(&msg)
}

/// Serialize a message into a complete v2 frame with the signing source identity.
pub(super) fn serialize_signing(msg: &MavMessage) -> Result<Vec<u8>, ()> {
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        // The router stamps its own sequence on its frames; a client-written
        // signing frame carries 0 (ArduPilot routes SETUP_SIGNING by target
        // regardless of the sequence).
        sequence: 0,
    };
    mavlink::serialize_v2(header, msg).map_err(|e| {
        tracing::error!(error = %e, "signing frame serialize failed");
    })
}

/// Overwrite a key buffer with zeros, mirroring the Python in-place zeroize so a
/// raw key never lingers in memory after the route returns.
pub(super) fn zeroize(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
}

/// Render a UTC `OffsetDateTime` as `YYYY-MM-DDTHH:MM:SS+00:00`, byte-for-byte
/// what the Python `datetime.now(tz=timezone.utc).isoformat(timespec="seconds")`
/// produces: a zero-padded date + time at seconds precision and the explicit
/// `+00:00` UTC offset (Python uses the numeric offset, never `Z`). Built from the
/// date/time fields directly so no format-description machinery is needed.
pub(super) fn iso8601_seconds_utc(dt: OffsetDateTime) -> String {
    let date = dt.date();
    let time = dt.time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time.hour(),
        time.minute(),
        time.second(),
    )
}
