//! Host-only MAVLink messages: no plugin may send them, whatever it holds.

use super::*;

#[test]
fn scan_outbound_flags_follow_target_as_host_only() {
    let scan = scan_outbound(&v2_frame(144)).expect("a whole frame");
    assert!(scan.host_only);
    assert!(
        !scan_outbound(&v2_frame(0))
            .expect("a whole frame")
            .host_only
    );
    // Behind a benign leading frame too.
    let mut buf = v2_frame(0);
    buf.extend_from_slice(&v2_frame_from(190, 144));
    assert!(scan_outbound(&buf).expect("whole frames").host_only);
}

#[test]
fn mavlink_send_refuses_follow_target_whatever_the_plugin_holds() {
    let host = RealHost::new();
    let every = caps(&[
        "mavlink.write",
        "estimator.pose.inject",
        "mavlink.component.vio",
        "follow.beacon.read",
    ]);
    for buf in [v2_frame(144), v2_frame_from(190, 144)] {
        let args = map(&[("msg_bytes", Value::Binary(buf))]);
        let body = err_body(host.mavlink_send("p", &args, &every));
        assert!(body.contains("only the agent may send"), "{body}");
    }
}
