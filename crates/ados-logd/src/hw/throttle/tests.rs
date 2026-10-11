use super::*;

#[test]
fn parses_a_typical_throttled_line() {
    assert_eq!(parse_get_throttled("throttled=0x50000\n"), Some(0x50000));
    assert_eq!(parse_get_throttled("throttled=0x0\n"), Some(0));
    assert_eq!(parse_get_throttled("throttled=0X5"), Some(5));
}

#[test]
fn rejects_output_without_a_value() {
    assert_eq!(parse_get_throttled("garbage"), None);
    assert_eq!(parse_get_throttled("throttled="), None);
    assert_eq!(parse_get_throttled("throttled=zz"), None);
}

#[test]
fn decodes_the_active_low_bits() {
    // 0x1 = under-voltage now; 0x50000 = high latched bits only (no active
    // low bit) -> all current flags clear.
    let now = decode_throttle(0x1);
    assert!(now.under_voltage);
    assert!(!now.freq_capped);

    let capped = decode_throttle(0x2);
    assert!(capped.freq_capped);

    let throttled = decode_throttle(0x4);
    assert!(throttled.throttled);

    let soft = decode_throttle(0x8);
    assert!(soft.soft_temp_limit);

    let clean = decode_throttle(0x50000);
    assert_eq!(clean.raw, 0x50000);
    assert!(!clean.under_voltage);
    assert!(!clean.freq_capped);
    assert!(!clean.throttled);
    assert!(!clean.soft_temp_limit);
}

#[tokio::test]
async fn non_pi_family_never_reads_throttle() {
    // SocFamily::Other is gated out before any subprocess spawn.
    assert_eq!(read_throttle(SocFamily::Other).await, None);
}
