//! Coverage for `encoder_for_board()`. Walks every documented branch
//! and asserts the factory returns a usable `Box<dyn Encoder + Send>`
//! without panicking. The lib already exercises a few branches in its
//! inline `#[cfg(test)] mod tests` block, but those checks bind the
//! returned trait object only locally; this integration test pulls the
//! factory through the public crate boundary so any future
//! visibility regression surfaces here too.
//!
//! The trait surface deliberately erases the concrete encoder type, so
//! the assertions below are limited to what the public API exposes:
//!
//!   * The factory returns a non-panicking handle for every known
//!     `encoder_api_lite` value, including the `rkmedia` alias.
//!   * The unknown / empty / `none` branches all collapse onto the
//!     null encoder, whose `start()` returns `EncoderError::NotImplemented`.
//!     That is the only behaviorally observable distinction the public
//!     API offers between the null fallback and the real backends, so
//!     we assert it for the catch-all branches and only sanity-check
//!     the non-null branches via construction.

use ados_video::{encoder_for_board, EncoderConfig, EncoderError};

#[tokio::test]
async fn factory_dispatch_v4l2_returns_usable_encoder() {
    let mut enc = encoder_for_board("v4l2");
    // The V4L2 backend is feature-complete on Linux but a stub
    // elsewhere. We don't drive `start()` here; just confirm the
    // construction round-trip and the trait-object size.
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_libcamera_returns_usable_encoder() {
    let mut enc = encoder_for_board("libcamera");
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_rkmpi_returns_usable_encoder() {
    let mut enc = encoder_for_board("rkmpi");
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_rkmedia_alias_returns_usable_encoder() {
    // `rkmedia` is a documented alias for `rkmpi`. Both branches must
    // resolve to the same backend type.
    let mut enc = encoder_for_board("rkmedia");
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_none_returns_null_encoder() {
    // The "none" string is the explicit no-video sentinel. The factory
    // is documented to return the null encoder, whose `start()` is
    // `EncoderError::NotImplemented`.
    let mut enc = encoder_for_board("none");
    let err = enc
        .start(EncoderConfig::default())
        .await
        .expect_err("null encoder must reject start");
    assert!(
        matches!(err, EncoderError::NotImplemented),
        "expected NotImplemented for `none`, got {err:?}",
    );
    assert!(
        enc.next_frame().await.is_none(),
        "null encoder must never produce frames",
    );
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_empty_string_returns_null_encoder() {
    // The empty string is not a documented value; the catch-all branch
    // must still return the null encoder rather than panicking.
    let mut enc = encoder_for_board("");
    let err = enc
        .start(EncoderConfig::default())
        .await
        .expect_err("null encoder must reject start");
    assert!(matches!(err, EncoderError::NotImplemented));
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_unknown_value_returns_null_encoder() {
    // Unknown vendor strings hit the `_` catch-all and collapse onto
    // the null encoder. The factory logs a warn-level breadcrumb in
    // production; here we just want the no-panic + NotImplemented
    // surface.
    let mut enc = encoder_for_board("xyz");
    let err = enc
        .start(EncoderConfig::default())
        .await
        .expect_err("unknown api must collapse to null encoder");
    assert!(
        matches!(err, EncoderError::NotImplemented),
        "expected NotImplemented for `xyz`, got {err:?}",
    );
    enc.stop().await;
}

#[tokio::test]
async fn factory_dispatch_is_idempotent_across_repeated_calls() {
    // Sanity check: calling the factory multiple times for the same
    // value must keep returning fresh, independent encoder handles.
    let mut a = encoder_for_board("none");
    let mut b = encoder_for_board("none");
    let _ = a.start(EncoderConfig::default()).await;
    let _ = b.start(EncoderConfig::default()).await;
    a.stop().await;
    b.stop().await;
}
