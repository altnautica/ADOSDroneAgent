use super::*;

#[test]
fn topics_match_the_python_strings() {
    assert_eq!(topic_mavlink_tx("d"), "ados/d/mavlink/tx");
    assert_eq!(topic_mavlink_rx("d"), "ados/d/mavlink/rx");
    assert_eq!(topic_msp_tx("d"), "ados/d/msp/tx");
    assert_eq!(topic_msp_rx("d"), "ados/d/msp/rx");
    assert_eq!(topic_vision_detections("d"), "ados/d/vision/detections");
    assert_eq!(topic_webrtc_offer("d"), "ados/d/webrtc/offer");
    assert_eq!(topic_webrtc_answer("d", "s1"), "ados/d/webrtc/answer/s1");
    assert_eq!(
        topic_plugin_stream("d", "com.example.a", "pose"),
        "ados/d/plugin/com.example.a/pose"
    );
}

#[test]
fn every_broker_lane_for_one_device_has_its_own_client_id() {
    // A duplicate ClientID makes the broker evict the sibling session, so
    // the lanes kick each other in a permanent loop rather than degrading.
    // The MAVLink relay holds the bare `ados-{id}`; every other lane suffixes.
    let ids = [
        relay_username("dev1"),
        msp_client_id("dev1"),
        format!("ados-{}-{WEBRTC_LANE}", "dev1"),
        format!("ados-{}-plugin-update", "dev1"),
        format!("ados-{}-{PLUGIN_PUBLISH_LANE}", "dev1"),
    ];
    assert_eq!(msp_client_id("dev1"), "ados-dev1-msp");
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "client ids must be distinct: {ids:?}"
    );
}
