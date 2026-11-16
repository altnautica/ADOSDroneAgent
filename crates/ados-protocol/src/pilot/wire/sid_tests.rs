use super::*;

fn body(v: serde_json::Value) -> Vec<u8> {
    rmp_serde::to_vec_named(&v).unwrap()
}

#[test]
fn a_frame_carries_its_instance_id() {
    let hb = body(serde_json::json!({"op": "hb", "src": "android", "sid": "4f2a"}));
    assert_eq!(surface_sid(&hb).as_deref(), Some("4f2a"));
    assert_eq!(
        decode_request(&hb).unwrap(),
        Request::Hb {
            src: "android".into()
        }
    );
}

#[test]
fn a_frame_without_a_usable_id_has_none() {
    let none = body(serde_json::json!({"op": "hb", "src": "mc"}));
    assert_eq!(surface_sid(&none), None);
    let empty = body(serde_json::json!({"op": "hb", "src": "mc", "sid": ""}));
    assert_eq!(surface_sid(&empty), None);
    let long = "a".repeat(SID_MAX_LEN + 1);
    let over = body(serde_json::json!({"op": "hb", "src": "mc", "sid": long}));
    assert_eq!(surface_sid(&over), None);
    let number = body(serde_json::json!({"op": "hb", "src": "mc", "sid": 7}));
    assert_eq!(surface_sid(&number), None);
}

#[test]
fn bye_decodes_from_a_surface_frame() {
    let (req, b) =
        json_to_request_body(&serde_json::json!({"op": "bye", "src": "mc", "sid": "t1"})).unwrap();
    assert_eq!(req, Request::Bye { src: "mc".into() });
    assert_eq!(surface_sid(&b).as_deref(), Some("t1"));
}
