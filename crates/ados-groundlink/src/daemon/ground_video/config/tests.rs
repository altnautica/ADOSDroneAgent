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
    let yaml = render(&["192.168.1.50".to_string()]);
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
