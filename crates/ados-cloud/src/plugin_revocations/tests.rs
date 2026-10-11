use super::*;
use ados_plugin_host::signing::load_revocations;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one HTTP response with `body`, returning the URL.
async fn serve_once(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf).await;
        let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    revoked_url(&format!("http://{addr}/"))
}

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config())
        .build()
        .unwrap()
}

#[test]
fn the_list_lives_under_the_registry_base() {
    assert_eq!(
        revoked_url("https://cloud.example.com/"),
        "https://cloud.example.com/v1/revoked.json"
    );
}

#[tokio::test]
async fn revocations_fetch_merges_add_only() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("plugin-revocations.json");
    std::fs::write(&local, r#"{"signers":["local-signer"],"archives":[]}"#).unwrap();
    let http = test_client();

    let url = serve_once(r#"{"signers":["remote-signer"],"archives":["ab12"]}"#).await;
    assert_eq!(fetch_and_merge(&http, &url, &local).await, Ok(true));
    let merged = load_revocations(&local).unwrap();
    assert!(merged.signers.contains("local-signer"));
    assert!(merged.signers.contains("remote-signer"));
    assert!(merged.archives.contains("ab12"));

    // A later list without those entries removes nothing.
    let url = serve_once(r#"{"signers":[],"archives":[]}"#).await;
    assert_eq!(fetch_and_merge(&http, &url, &local).await, Ok(false));
    let after = load_revocations(&local).unwrap();
    assert_eq!(after.signers, merged.signers);
    assert_eq!(after.archives, merged.archives);
}

#[tokio::test]
async fn the_registry_shape_merges_signers_and_versions() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("plugin-revocations.json");
    std::fs::write(&local, r#"{"signers":["local-signer"],"archives":[]}"#).unwrap();
    // The exact body the registry's `/v1/revoked.json` serves.
    let url = serve_once(
            r#"{"signer_keys":["remote-signer"],"plugin_versions":["com.example.thermal@1.2.0"],"generated_at":1760000000000}"#,
        )
        .await;
    assert_eq!(
        fetch_and_merge(&test_client(), &url, &local).await,
        Ok(true)
    );
    let merged = load_revocations(&local).unwrap();
    assert!(merged.signers.contains("local-signer"));
    assert!(merged.signers.contains("remote-signer"));
    assert!(merged.version_revoked("com.example.thermal", "1.2.0"));
}

#[tokio::test]
async fn a_malformed_list_is_an_error_and_leaves_the_local_list() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("plugin-revocations.json");
    std::fs::write(&local, r#"{"signers":["local-signer"],"archives":[]}"#).unwrap();
    let url = serve_once("not json").await;
    assert!(fetch_and_merge(&test_client(), &url, &local).await.is_err());
    let kept = load_revocations(&local).unwrap();
    assert!(kept.signers.contains("local-signer"));
}
