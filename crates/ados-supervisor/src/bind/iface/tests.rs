use super::*;

#[test]
fn missing_iface_counter_is_none() {
    // A guaranteed-absent iface name.
    assert!(read_rx_packets_counter("ados-no-such-iface-xyz").is_none());
}

#[tokio::test]
async fn wait_returns_false_quickly_for_absent_iface() {
    // 0-budget: the loop body runs zero times → immediate false, no spawn.
    let got = wait_for_iface("ados-no-such-iface-xyz", Duration::from_millis(0)).await;
    assert!(!got);
}
