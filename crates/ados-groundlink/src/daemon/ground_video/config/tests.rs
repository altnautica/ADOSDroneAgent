use super::*;

const GOLDEN_PATHS: &str = "\
main:
  source: udp+rtp://127.0.0.1:5600
  rtpSDP: |
    v=0
    o=- 0 0 IN IP4 127.0.0.1
    s=ados-ground
    c=IN IP4 127.0.0.1
    t=0 0
    m=video 5600 RTP/AVP 96
    a=rtpmap:96 H264/90000
";

#[test]
fn gs_path_config_golden() {
    let yaml = render(&["192.168.1.50".to_string()], false);
    let doc: Value = serde_norway::from_str(&yaml).unwrap();
    let golden: Value = serde_norway::from_str(GOLDEN_PATHS).unwrap();
    assert_eq!(doc["paths"], golden);
    // Ground-profile values from the shared generator survive the rewrite.
    assert_eq!(doc["hlsVariant"], Value::String("mpegts".into()));
    assert_eq!(doc["webrtcAddress"], Value::String("127.0.0.1:8889".into()));
    assert_eq!(doc["rtspTransports"][0], Value::String("tcp".into()));
    assert_eq!(
        doc["webrtcAdditionalHosts"][0],
        Value::String("192.168.1.50".into())
    );
    assert!(!yaml.contains("sourceOnDemand"));
}

/// A ground station on somebody's own computer with sharing off opens nothing on
/// their network: the publish leg, the ICE socket and the advertised candidates
/// are all loopback, and no interface is scanned for candidates.
#[test]
fn a_loopback_ground_config_binds_and_advertises_only_loopback() {
    let yaml = render(&["192.168.1.50".to_string()], true);
    let doc: Value = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(doc["rtspAddress"], Value::String("127.0.0.1:8554".into()));
    assert_eq!(doc["webrtcAddress"], Value::String("127.0.0.1:8889".into()));
    assert_eq!(
        doc["webrtcLocalUDPAddress"],
        Value::String("127.0.0.1:8189".into())
    );
    assert_eq!(doc["webrtcIPsFromInterfaces"], Value::Bool(false));
    assert_eq!(
        doc["webrtcAdditionalHosts"],
        serde_norway::from_str::<Value>("[127.0.0.1]").unwrap()
    );
    // The pass-through LAN IP is not advertised, whatever was detected.
    assert!(!yaml.contains("192.168.1.50"));
    // The decode path is unchanged: the radio fan-out still delivers on loopback.
    assert_eq!(
        doc["paths"]["main"]["source"],
        Value::String("udp+rtp://127.0.0.1:5600".into())
    );
}
