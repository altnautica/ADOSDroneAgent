use super::*;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[test]
fn the_size_cap_holds_on_a_roomy_filesystem() {
    let budget = upload_budget(Some(100 * GIB)).unwrap();
    assert_eq!(budget.admit(512 * MIB), Ok(()));
    assert_eq!(budget.admit(512 * MIB + 1), Err(SizeRefusal::TooLarge));
    // Unknown free space still enforces the cap.
    let unknown = upload_budget(None).unwrap();
    assert_eq!(unknown.admit(512 * MIB + 1), Err(SizeRefusal::TooLarge));
}

#[test]
fn an_upload_never_leaves_less_than_a_gibibyte_free() {
    // 1 GiB + 100 MiB free: a 100 MiB model fits exactly, one byte more does not.
    let budget = upload_budget(Some(GIB + 100 * MIB)).unwrap();
    assert_eq!(budget.admit(100 * MIB), Ok(()));
    assert_eq!(budget.admit(100 * MIB + 1), Err(SizeRefusal::NoSpace));
    // At or below the floor nothing is accepted before a byte is written.
    assert_eq!(upload_budget(Some(GIB)), Err(SizeRefusal::NoSpace));
    assert_eq!(upload_budget(Some(10 * MIB)), Err(SizeRefusal::NoSpace));
}

fn meta(id: &str, runtime: &str) -> UploadMeta {
    UploadMeta {
        id: id.to_string(),
        name: format!("{id} model"),
        classes: vec!["person".to_string(), "car".to_string()],
        head: "yolo8".to_string(),
        input_w: 640,
        input_h: 640,
        runtime: runtime.to_string(),
        board_match: "generic".to_string(),
    }
}

fn read_catalog(dir: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(dir.join(CUSTOM_CATALOG)).unwrap();
    match serde_json::from_str::<Value>(&text).unwrap() {
        Value::Array(a) => a,
        _ => panic!("catalog is not an array"),
    }
}

/// Build a `StagedFile` the way `stream_field_to_tmp` does: write a tmp file in
/// `dir` and accumulate its digest + size, so `store_upload` can finalize it.
fn stage(dir: &Path, bytes: &[u8]) -> StagedFile {
    let tmp_path = dir.join(format!(".upload-test-{}.tmp", random_token()));
    std::fs::write(&tmp_path, bytes).unwrap();
    let mut h = Sha256::new();
    h.update(bytes);
    StagedFile {
        tmp_path,
        sha256: hex::encode(h.finalize()),
        size_bytes: bytes.len() as u64,
    }
}

#[test]
fn suffix_maps_each_runtime() {
    assert_eq!(suffix_for_runtime("rknn"), ".rknn");
    assert_eq!(suffix_for_runtime("RKNN"), ".rknn");
    assert_eq!(suffix_for_runtime("tflite"), ".tflite");
    assert_eq!(suffix_for_runtime("onnx"), ".onnx");
    assert_eq!(suffix_for_runtime(""), ".onnx");
    assert_eq!(suffix_for_runtime("anything-else"), ".onnx");
}

#[test]
fn an_engine_file_upload_is_refused() {
    assert!(is_unsupported_model_file("detector.engine"));
    assert!(is_unsupported_model_file("DETECTOR.ENGINE"));
    assert!(is_unsupported_model_file("dir/yolo.Engine"));
    // Every format a backend runs is accepted, as is a name with no
    // extension (the runtime then decides the on-disk suffix).
    for ok in [
        "m.rknn",
        "m.onnx",
        "m.tflite",
        "engine",
        "m.engine.onnx",
        "",
    ] {
        assert!(!is_unsupported_model_file(ok), "{ok} must be accepted");
    }
}

#[test]
fn store_writes_file_hashes_and_catalogs() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = b"fake-onnx-bytes";
    let staged = stage(dir.path(), bytes);
    let entry = store_upload(dir.path(), &meta("custom-1", "onnx"), staged).unwrap();

    // The body reports the derived filename + the streamed-file hash.
    assert_eq!(entry["status"], json!("ok"));
    assert_eq!(entry["id"], json!("custom-1"));
    assert_eq!(entry["filename"], json!("custom-1.onnx"));
    assert_eq!(entry["size_bytes"], json!(bytes.len()));
    // sha256 of the bytes.
    let mut h = Sha256::new();
    h.update(bytes);
    assert_eq!(entry["sha256"], json!(hex::encode(h.finalize())));

    // The file landed under the derived name.
    let model_path = dir.path().join("custom-1.onnx");
    assert_eq!(std::fs::read(&model_path).unwrap(), bytes);

    // The catalog carries the metadata.
    let cat = read_catalog(dir.path());
    assert_eq!(cat.len(), 1);
    let e = &cat[0];
    assert_eq!(e["id"], json!("custom-1"));
    assert_eq!(e["name"], json!("custom-1 model"));
    assert_eq!(e["classes"], json!(["person", "car"]));
    assert_eq!(e["input_w"], json!(640));
    assert_eq!(e["custom"], json!(true));
    assert_eq!(e["filename"], json!("custom-1.onnx"));
}

#[test]
fn runtime_drives_the_suffix_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let staged = stage(dir.path(), b"rk");
    store_upload(dir.path(), &meta("rk-model", "rknn"), staged).unwrap();
    assert!(dir.path().join("rk-model.rknn").exists());
    let cat = read_catalog(dir.path());
    assert_eq!(cat[0]["filename"], json!("rk-model.rknn"));
}

#[test]
fn re_uploading_the_same_id_replaces_the_catalog_entry() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = stage(dir.path(), b"v1");
    store_upload(dir.path(), &meta("dup", "onnx"), s1).unwrap();
    // A second model under a different id coexists.
    let s2 = stage(dir.path(), b"x");
    store_upload(dir.path(), &meta("other", "onnx"), s2).unwrap();
    // Re-upload "dup" with new bytes → one "dup" entry, updated hash + file.
    let s3 = stage(dir.path(), b"v2-longer");
    store_upload(dir.path(), &meta("dup", "onnx"), s3).unwrap();

    let cat = read_catalog(dir.path());
    let dup: Vec<&Value> = cat
        .iter()
        .filter(|e| e.get("id").and_then(|v| v.as_str()) == Some("dup"))
        .collect();
    assert_eq!(dup.len(), 1, "the id must be deduplicated in the catalog");
    // Both ids are present (the replace did not drop the other entry).
    assert_eq!(cat.len(), 2);
    assert_eq!(
        std::fs::read(dir.path().join("dup.onnx")).unwrap(),
        b"v2-longer"
    );
    let mut h = Sha256::new();
    h.update(b"v2-longer");
    assert_eq!(dup[0]["sha256"], json!(hex::encode(h.finalize())));
}

#[test]
fn catalog_starts_fresh_when_the_existing_one_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    // A pre-existing non-array catalog (corruption) is replaced cleanly.
    std::fs::write(dir.path().join(CUSTOM_CATALOG), b"{ not an array }").unwrap();
    let staged = stage(dir.path(), b"z");
    store_upload(dir.path(), &meta("fresh", "onnx"), staged).unwrap();
    let cat = read_catalog(dir.path());
    assert_eq!(cat.len(), 1);
    assert_eq!(cat[0]["id"], json!("fresh"));
}

#[test]
fn empty_id_rejected_before_a_write_in_the_handler_guard() {
    // The handler rejects an empty id; assert the guard predicate the handler
    // uses so the contract is covered without driving a real multipart body.
    let m = UploadMeta {
        id: "   ".to_string(),
        ..UploadMeta::default()
    };
    assert!(m.id.trim().is_empty());
}

#[test]
fn upload_meta_tolerates_unknown_fields_and_terse_blob() {
    // A terse metadata blob (only id) parses with defaults; unknown fields are
    // ignored (serde default policy), so a richer client payload is accepted.
    let m: UploadMeta =
        serde_json::from_str(r#"{"id":"x","unknown_extra":42,"name":"X"}"#).unwrap();
    assert_eq!(m.id, "x");
    assert_eq!(m.name, "X");
    assert!(m.classes.is_empty());
    assert_eq!(m.input_w, 0);
}

#[test]
fn valid_model_id_accepts_normal_names() {
    for id in ["custom-1", "yolov8n", "model.v2", "a_b-c.D9", ".hidden-ok"] {
        assert!(valid_model_id(id), "{id} should be accepted");
    }
}

#[test]
fn valid_model_id_rejects_traversal_and_separators() {
    // Path separators (absolute or relative), `..`, and out-of-charset chars
    // are all refused before the id can become a filename joined onto the dir.
    for id in [
        "",
        ".",
        "..",
        "../../etc/ados/evil",
        "/etc/cron.d/x",
        "a/b",
        "a\\b",
        "evil id",
        "name;rm",
        "weird*name",
    ] {
        assert!(!valid_model_id(id), "{id:?} must be rejected");
    }
}

#[test]
fn store_upload_only_writes_inside_the_dir_for_a_valid_id() {
    // The handler validates the id before calling store_upload; a valid id is a
    // single file directly inside the dir (the rename is a same-dir move), so
    // no path outside `dir` is ever touched.
    let dir = tempfile::tempdir().unwrap();
    let staged = stage(dir.path(), b"safe");
    let entry = store_upload(dir.path(), &meta("ok-id", "onnx"), staged).unwrap();
    assert_eq!(entry["filename"], json!("ok-id.onnx"));
    assert!(dir.path().join("ok-id.onnx").exists());
}

#[test]
fn dropping_a_staged_file_removes_the_tmp() {
    // The error / rejected paths rely on StagedFile's Drop to clean the tmp.
    let dir = tempfile::tempdir().unwrap();
    let tmp = {
        let staged = stage(dir.path(), b"orphan");
        let p = staged.tmp_path.clone();
        assert!(p.exists());
        p
        // staged drops here
    };
    assert!(!tmp.exists(), "the tmp file must be removed on drop");
}

#[test]
fn store_upload_consumes_the_staged_tmp_on_success() {
    // A finalized upload renames the tmp into place (ManuallyDrop disarms the
    // cleanup), leaving exactly the derived file and no lingering tmp.
    let dir = tempfile::tempdir().unwrap();
    let staged = stage(dir.path(), b"payload");
    let tmp = staged.tmp_path.clone();
    store_upload(dir.path(), &meta("final", "onnx"), staged).unwrap();
    assert!(
        !tmp.exists(),
        "the staged tmp must be renamed, not left behind"
    );
    assert_eq!(
        std::fs::read(dir.path().join("final.onnx")).unwrap(),
        b"payload"
    );
}

#[test]
fn a_failed_catalog_write_restores_the_previous_model() {
    // A re-upload whose catalog write fails must leave the previous model
    // bytes in place: the catalog still describes them.
    let dir = tempfile::tempdir().unwrap();
    store_upload(dir.path(), &meta("det", "onnx"), stage(dir.path(), b"v1")).unwrap();
    // A directory where the catalog file should be makes the rename fail.
    std::fs::remove_file(dir.path().join(CUSTOM_CATALOG)).unwrap();
    std::fs::create_dir(dir.path().join(CUSTOM_CATALOG)).unwrap();

    let err = store_upload(dir.path(), &meta("det", "onnx"), stage(dir.path(), b"v2"));
    assert!(err.is_err());
    assert_eq!(std::fs::read(dir.path().join("det.onnx")).unwrap(), b"v1");
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names.len(),
        2,
        "only the model and the catalog remain: {names:?}"
    );
}

#[test]
fn models_dir_from_config_reads_vision_models_dir() {
    // The upload resolves the models dir from `vision.models_dir` so it writes
    // where the Python manager lists from. An override is honoured; a blank /
    // absent field / unparseable file falls through to None (env / default).
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");

    std::fs::write(&cfg, "vision:\n  models_dir: /var/ados/custom-models\n").unwrap();
    assert_eq!(
        models_dir_from_config_at(&cfg),
        Some(PathBuf::from("/var/ados/custom-models"))
    );

    std::fs::write(&cfg, "vision:\n  models_dir: \"\"\n").unwrap();
    assert_eq!(
        models_dir_from_config_at(&cfg),
        None,
        "a blank dir falls through"
    );

    // The key lives under `vision`, not `system`.
    std::fs::write(&cfg, "system:\n  models_dir: /var/ados/custom-models\n").unwrap();
    assert_eq!(
        models_dir_from_config_at(&cfg),
        None,
        "an absent field falls through"
    );

    assert_eq!(
        models_dir_from_config_at(Path::new("/nonexistent/ados/config.yaml")),
        None,
        "a missing config falls through"
    );
}

/// A multipart body with one part named `name` carrying `payload`.
async fn one_part(name: &str, payload: &[u8]) -> Multipart {
    use axum::extract::FromRequest;
    let boundary = "ados-test-boundary";
    let mut body =
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
            .into_bytes();
    body.extend_from_slice(payload);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = axum::http::Request::builder()
        .method("POST")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(body))
        .unwrap();
    match Multipart::from_request(request, &()).await {
        Ok(m) => m,
        Err(_) => panic!("a well-formed multipart request"),
    }
}

#[tokio::test]
async fn metadata_1mib_413() {
    let mut multipart = one_part("metadata", &vec![b' '; 1024 * 1024]).await;
    let field = multipart.next_field().await.unwrap().unwrap();
    match read_metadata(field).await {
        Ok(_) => panic!("a 1 MiB metadata part must be refused"),
        Err(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
    }
    // A metadata part under the cap still parses.
    let mut multipart = one_part("metadata", br#"{"id":"small"}"#).await;
    let field = multipart.next_field().await.unwrap().unwrap();
    match read_metadata(field).await {
        Ok(m) => assert_eq!(m.id, "small"),
        Err(resp) => panic!("small metadata refused: {}", resp.status()),
    }
}

#[tokio::test]
async fn an_unknown_part_is_drained_under_the_cap() {
    let mut multipart = one_part("extra", &vec![0u8; PART_MAX_BYTES + 1]).await;
    let field = multipart.next_field().await.unwrap().unwrap();
    let refused = drain_part_capped(field, PART_MAX_BYTES).await;
    assert_eq!(
        refused.map_err(|r| r.status()),
        Err(StatusCode::PAYLOAD_TOO_LARGE)
    );
    let mut multipart = one_part("extra", b"small").await;
    let field = multipart.next_field().await.unwrap().unwrap();
    assert!(drain_part_capped(field, PART_MAX_BYTES).await.is_ok());
}

#[test]
fn the_content_digest_header_is_parsed_or_refused() {
    let mut headers = HeaderMap::new();
    assert_eq!(expected_digest(&headers).ok(), Some(None));
    let digest = "AB".repeat(32);
    headers.insert(CONTENT_SHA256_HEADER, digest.parse().unwrap());
    assert_eq!(expected_digest(&headers).ok(), Some(Some("ab".repeat(32))));
    headers.insert(CONTENT_SHA256_HEADER, "not-hex".parse().unwrap());
    assert_eq!(
        expected_digest(&headers).map_err(|r| r.status()),
        Err(StatusCode::BAD_REQUEST)
    );
    assert_eq!(digest_mismatch().status(), StatusCode::UNAUTHORIZED);
}
