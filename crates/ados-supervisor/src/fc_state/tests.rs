use super::*;
use ados_protocol::state::{encode_v1, encode_v2};
use serde_json::json;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn armed_from_state_reads_both_wire_forms_fail_closed() {
    // v2 (length-prefixed msgpack), the form the producer actually emits.
    let mut armed_v2 = std::io::Cursor::new(encode_v2(&json!({"armed": true})).unwrap());
    assert_eq!(armed_from_state(&mut armed_v2).await, Some(true));

    let mut disarmed_v2 =
        std::io::Cursor::new(encode_v2(&json!({"armed": false, "mode": "GUIDED"})).unwrap());
    assert_eq!(armed_from_state(&mut disarmed_v2).await, Some(false));

    // v1 (newline JSON), the legacy form, still read via the auto-detect.
    let mut armed_v1 = std::io::Cursor::new(encode_v1(&json!({"armed": true})).unwrap());
    assert_eq!(armed_from_state(&mut armed_v1).await, Some(true));

    // Fail-closed: an empty reader (clean EOF) yields None.
    let mut empty = std::io::Cursor::new(Vec::<u8>::new());
    assert_eq!(armed_from_state(&mut empty).await, None);

    // Fail-closed: a snapshot with no `armed` field yields None.
    let mut no_field = std::io::Cursor::new(encode_v2(&json!({"mode": "STABILIZE"})).unwrap());
    assert_eq!(armed_from_state(&mut no_field).await, None);
}

#[tokio::test]
async fn read_fc_armed_at_reads_a_fake_state_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("state.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let frame = encode_v2(&json!({"armed": false})).unwrap();
        conn.write_all(&frame).await.unwrap();
    });
    assert_eq!(read_fc_armed_at(&sock).await, Some(false));
    server.await.unwrap();
}

#[tokio::test]
async fn read_fc_armed_at_is_none_without_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        read_fc_armed_at(&dir.path().join("absent.sock")).await,
        None
    );
}
