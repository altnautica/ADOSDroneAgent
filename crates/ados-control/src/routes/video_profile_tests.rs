use super::*;
use ados_video::profile::lease::LeaseView;
use ados_video::profile::EncoderSettings;
use axum::body::Body;
use axum::http::Request;
use tower::util::ServiceExt;

fn hero_state() -> EncoderState {
    EncoderState::new(
        VideoProfile::Hero,
        None,
        EncoderSettings {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
        },
    )
}

fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn the_response_reports_what_was_applied_including_an_active_clamp() {
    let clamped = EncoderState::new(
        VideoProfile::Hero,
        Some(1200),
        EncoderSettings {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 1200,
        },
    );
    let body = applied_body(&clamped);
    assert_eq!(body["profile"], "hero");
    assert_eq!(body["width"], 1280);
    assert_eq!(body["fps"], 30);
    // The applied bitrate is the clamped one, not the profile's nominal
    // 4000 — reporting 4000 here would tell the operator the link is fine.
    assert_eq!(body["bitrate_kbps"], 1200);
    assert_eq!(body["ceiling_kbps"], 1200);
}

#[test]
fn an_unclamped_thumbnail_reports_a_null_ceiling() {
    let st = EncoderState::new(
        VideoProfile::Thumbnail,
        None,
        EncoderSettings {
            width: 320,
            height: 180,
            fps: 1,
            bitrate_kbps: 50,
        },
    );
    let body = applied_body(&st);
    assert_eq!(body["profile"], "thumbnail");
    assert_eq!(body["bitrate_kbps"], 50);
    assert!(body["ceiling_kbps"].is_null());
    assert_eq!(body["leases"], json!([]));
}

#[test]
fn the_response_carries_the_base_profile_and_the_leases() {
    let mut st = hero_state();
    st.base_profile = Some(VideoProfile::Thumbnail);
    st.leases = vec![
        LeaseView {
            holder: "gcs-1".to_string(),
            expires_in_s: Some(12.5),
        },
        LeaseView {
            holder: "vision".to_string(),
            expires_in_s: None,
        },
    ];
    st.encoder_respawns = 3;
    let body = applied_body(&st);
    assert_eq!(body["profile"], "hero");
    assert_eq!(body["base_profile"], "thumbnail");
    assert_eq!(body["encoder_respawns"], 3);
    assert_eq!(body["ceiling_deferred"], false);
    assert_eq!(
        body["leases"],
        json!([
            {"holder": "gcs-1", "expires_in_s": 12.5},
            {"holder": "vision", "expires_in_s": null},
        ])
    );

    // A video service that predates leases reports no base: null, not a guess.
    st.base_profile = None;
    assert!(applied_body(&st)["base_profile"].is_null());
}

#[test]
fn a_body_without_a_holder_is_the_plain_set() {
    assert_eq!(
        parse_post(&json!({"profile": "hero"})),
        Ok(ProfileRequest::Set(VideoProfile::Hero))
    );
    assert_eq!(
        parse_post(&json!({"profile": "thumbnail", "holder": null})),
        Ok(ProfileRequest::Set(VideoProfile::Thumbnail))
    );
    // A lease length without a holder changes nothing about the plain set.
    assert_eq!(
        parse_post(&json!({"profile": "hero", "lease_s": 500})),
        Ok(ProfileRequest::Set(VideoProfile::Hero))
    );
    for bad in [
        json!({}),
        json!({"profile": "Hero"}),
        json!({"profile": 1}),
        Value::Null,
    ] {
        assert!(parse_post(&bad).is_err(), "{bad}");
    }
}

#[test]
fn a_hero_body_with_a_holder_is_a_lease_defaulting_to_30_s() {
    assert_eq!(
        parse_post(&json!({"profile": "hero", "holder": "gcs-1"})),
        Ok(ProfileRequest::Lease {
            holder: "gcs-1".to_string(),
            lease_s: 30,
        })
    );
    assert_eq!(
        parse_post(&json!({"profile": "hero", "holder": "gcs-1", "lease_s": null})),
        Ok(ProfileRequest::Lease {
            holder: "gcs-1".to_string(),
            lease_s: 30,
        })
    );
    for lease_s in [5, 120] {
        assert_eq!(
            parse_post(&json!({"profile": "hero", "holder": "mc:tab.7_x", "lease_s": lease_s})),
            Ok(ProfileRequest::Lease {
                holder: "mc:tab.7_x".to_string(),
                lease_s,
            })
        );
    }
}

#[test]
fn a_lease_length_outside_5_to_120_or_not_an_integer_is_rejected() {
    for lease_s in [
        json!(4),
        json!(121),
        json!(0),
        json!(-5),
        json!(30.5),
        json!("30"),
        json!(true),
    ] {
        let body = json!({"profile": "hero", "holder": "gcs-1", "lease_s": lease_s});
        assert!(parse_post(&body).is_err(), "{body}");
    }
}

#[test]
fn a_holder_outside_the_charset_or_length_is_rejected() {
    let longest = "a".repeat(64);
    assert!(parse_post(&json!({"profile": "hero", "holder": longest})).is_ok());
    let too_long = "a".repeat(65);
    for holder in [
        json!(""),
        json!(too_long),
        json!("has space"),
        json!("a/b"),
        json!("caf\u{e9}"),
        json!(7),
        json!("vision"),
    ] {
        let body = json!({"profile": "hero", "holder": holder});
        assert!(parse_post(&body).is_err(), "{body}");
    }
}

#[test]
fn a_thumbnail_body_with_a_holder_releases_that_holder() {
    assert_eq!(
        parse_post(&json!({"profile": "thumbnail", "holder": "gcs-1"})),
        Ok(ProfileRequest::Release {
            holder: "gcs-1".to_string(),
        })
    );
    // The release ignores a lease length rather than validating it.
    assert_eq!(
        parse_post(&json!({"profile": "thumbnail", "holder": "gcs-1", "lease_s": 999})),
        Ok(ProfileRequest::Release {
            holder: "gcs-1".to_string(),
        })
    );
    assert!(parse_post(&json!({"profile": "thumbnail", "holder": "a b"})).is_err());
}

#[test]
fn a_release_query_needs_a_valid_holder() {
    assert_eq!(
        parse_release_query(&query(&[("holder", "gcs-1")])),
        Ok("gcs-1".to_string())
    );
    assert!(parse_release_query(&query(&[])).is_err());
    assert!(parse_release_query(&query(&[("holder", "")])).is_err());
    assert!(parse_release_query(&query(&[("holder", "a b")])).is_err());
    assert!(parse_release_query(&query(&[("other", "gcs-1")])).is_err());
}

/// Send one request through a freshly built router on a node of `profile`.
async fn send(profile: &str, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let (_dir, state) = crate::routes::recording_route_tests::state_for_profile(profile);
    let router = crate::routes::build_router(state, false);
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn a_ground_station_answers_profile_mismatch_on_every_method() {
    for (method, uri, body) in [
        ("GET", "/api/video/profile", ""),
        ("POST", "/api/video/profile", r#"{"profile":"hero"}"#),
        (
            "POST",
            "/api/video/profile",
            r#"{"profile":"hero","holder":"gcs-1","lease_s":30}"#,
        ),
        ("DELETE", "/api/video/profile?holder=gcs-1", ""),
    ] {
        let (status, text) = send("ground_station", method, uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {text}");
        assert!(
            text.contains("E_PROFILE_MISMATCH"),
            "{method} {uri} missed its handler: {text}"
        );
    }
}

#[tokio::test]
async fn a_drone_rejects_a_bad_lease_request_before_touching_the_encoder() {
    for (method, uri, body) in [
        ("DELETE", "/api/video/profile", ""),
        ("DELETE", "/api/video/profile?holder=", ""),
        ("DELETE", "/api/video/profile?holder=a%20b", ""),
        (
            "POST",
            "/api/video/profile",
            r#"{"profile":"hero","holder":"gcs-1","lease_s":121}"#,
        ),
        (
            "POST",
            "/api/video/profile",
            r#"{"profile":"hero","holder":"a/b"}"#,
        ),
    ] {
        let (status, text) = send("drone", method, uri, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: {text}");
        assert!(text.contains("detail"), "{method} {uri}: {text}");
    }
}
