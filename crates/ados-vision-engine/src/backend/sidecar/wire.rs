//! The sidecar wire protocol: 4-byte BE length + msgpack named map.

use super::*;

pub(super) fn fmt_str(f: FrameFormat) -> &'static str {
    match f {
        FrameFormat::Rgb24 => "rgb24",
        FrameFormat::Nv12 => "nv12",
        FrameFormat::Yuv420p => "yuv420p",
        FrameFormat::Gray8 => "gray8",
        FrameFormat::Gray16 => "gray16",
        FrameFormat::BayerRggb8 => "bayerrggb8",
        FrameFormat::BayerRggb16 => "bayerrggb16",
    }
}

/// The sidecar's lowercase name for a detection head, so it decodes the right
/// output-tensor layout.
pub(super) fn head_str(h: DetectionHead) -> &'static str {
    match h {
        DetectionHead::Yolo8 => "yolov8",
        DetectionHead::Yolo5 => "yolov5",
    }
}

pub(in crate::backend) fn mv(s: &str) -> rmpv::Value {
    rmpv::Value::from(s)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn load_request(
    model_id: &str,
    path: &str,
    input_w: u32,
    input_h: u32,
    format: &str,
    classes: &[String],
    head: &str,
) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (mv("op"), mv("load_model")),
        (mv("model_id"), mv(model_id)),
        (mv("path"), mv(path)),
        (mv("input_w"), rmpv::Value::from(input_w as u64)),
        (mv("input_h"), rmpv::Value::from(input_h as u64)),
        (mv("format"), mv(format)),
        (
            mv("class_labels"),
            rmpv::Value::Array(classes.iter().map(|c| mv(c)).collect()),
        ),
        (mv("head"), mv(head)),
    ])
}

/// A request that runs a resident model on one raw frame: `infer` (a
/// detector, replying with detections) or `dense` (a depth model, replying
/// with its per-pixel map).
pub(super) fn frame_request(
    op: &str,
    model_id: &str,
    frame: &[u8],
    width: u32,
    height: u32,
    format: &str,
) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (mv("op"), mv(op)),
        (mv("model_id"), mv(model_id)),
        // bytes MUST be a msgpack bin, not an int array, so the sidecar can
        // np.frombuffer it.
        (mv("frame"), rmpv::Value::Binary(frame.to_vec())),
        (mv("width"), rmpv::Value::from(width as u64)),
        (mv("height"), rmpv::Value::from(height as u64)),
        (mv("format"), mv(format)),
    ])
}

/// Decode a `{status, width, height, data}` dense reply: `data` is
/// `width * height` little-endian f32 values, row-major.
pub(super) fn decode_dense(resp: &rmpv::Value) -> Result<DenseMap> {
    check_ok(resp)?;
    let map = resp
        .as_map()
        .ok_or_else(|| anyhow!("response is not a map"))?;
    let dim = |k: &str| {
        map_get(map, k)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| anyhow!("dense response has no {k}"))
    };
    let (w, h) = (dim("width")?, dim("height")?);
    let Some(rmpv::Value::Binary(bytes)) = map_get(map, "data") else {
        return Err(anyhow!("dense response has no binary data"));
    };
    if bytes.len() % 4 != 0 {
        return Err(anyhow!("dense data of {} bytes is not f32", bytes.len()));
    }
    let data = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    DenseMap::new(w, h, data)
}

/// An `embed` request: a pre-cropped, model-input-sized rgb24 crop the sidecar
/// runs through a resident re-id model, returning the embedding. The crop is a
/// msgpack bin (same as `infer`'s frame) so the sidecar can `np.frombuffer` it.
pub(super) fn embed_request(
    model_id: &str,
    crop: &[u8],
    width: u32,
    height: u32,
    format: &str,
) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (mv("op"), mv("embed")),
        (mv("model_id"), mv(model_id)),
        (mv("crop"), rmpv::Value::Binary(crop.to_vec())),
        (mv("crop_w"), rmpv::Value::from(width as u64)),
        (mv("crop_h"), rmpv::Value::from(height as u64)),
        (mv("format"), mv(format)),
    ])
}

/// Decode an `{status, embedding}` reply into a flat f32 embedding.
pub(super) fn decode_embedding(resp: &rmpv::Value) -> Result<Vec<f32>> {
    check_ok(resp)?;
    let map = resp
        .as_map()
        .ok_or_else(|| anyhow!("response is not a map"))?;
    let arr = map_get(map, "embedding")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("embed response has no embedding array"))?;
    // Error (not silently shorten) on a non-numeric element: a truncated
    // embedding would not match the template length and would silently fail the
    // appearance gate. The engine treats this Err as motion-only for the box.
    arr.iter()
        .map(|v| num_f32(v).ok_or_else(|| anyhow!("embedding has a non-numeric element")))
        .collect()
}

/// One blocking framed request/response against the sidecar socket. A fresh
/// connection per call; the sidecar keys loaded models by id across connections.
pub(super) fn round_trip(socket_path: &str, req: &rmpv::Value) -> Result<rmpv::Value> {
    let mut body = Vec::new();
    rmpv::encode::write_value(&mut body, req).context("encode request")?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(anyhow!(
            "request body {} exceeds {MAX_FRAME_BYTES}",
            body.len()
        ));
    }
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect sidecar at {socket_path}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let n = u32::from_be_bytes(len_buf) as usize;
    if n > MAX_FRAME_BYTES {
        return Err(anyhow!("response body {n} exceeds {MAX_FRAME_BYTES}"));
    }
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    rmpv::decode::read_value(&mut &buf[..]).context("decode response")
}

pub(in crate::backend) fn map_get<'a>(
    map: &'a [(rmpv::Value, rmpv::Value)],
    key: &str,
) -> Option<&'a rmpv::Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

pub(super) fn num_f32(v: &rmpv::Value) -> Option<f32> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_u64().map(|u| u as f64))
        .map(|f| f as f32)
}

pub(super) fn parse_lock(s: &str) -> Option<LockState> {
    match s {
        "locked" => Some(LockState::Locked),
        "uncertain" => Some(LockState::Uncertain),
        "lost" => Some(LockState::Lost),
        _ => None,
    }
}

pub(super) fn check_ok(resp: &rmpv::Value) -> Result<()> {
    let map = resp
        .as_map()
        .ok_or_else(|| anyhow!("response is not a map"))?;
    match map_get(map, "status").and_then(|v| v.as_str()) {
        Some("ok") => Ok(()),
        _ => {
            let err = map_get(map, "error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("{err}"))
        }
    }
}

pub(super) fn is_not_loaded(e: &anyhow::Error) -> bool {
    e.to_string().to_ascii_lowercase().contains("not loaded")
}

/// Decode an `{status, detections}` reply into the wire `Detection` shape.
pub(super) fn decode_detections(resp: &rmpv::Value) -> Result<Vec<Detection>> {
    check_ok(resp)?;
    let map = resp
        .as_map()
        .ok_or_else(|| anyhow!("response is not a map"))?;
    let dets = match map_get(map, "detections").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(dets.len());
    for d in dets {
        let dm = d
            .as_map()
            .ok_or_else(|| anyhow!("detection is not a map"))?;
        let bm = map_get(dm, "bbox")
            .and_then(|v| v.as_map())
            .ok_or_else(|| anyhow!("detection has no bbox map"))?;
        let bf = |k: &str| map_get(bm, k).and_then(num_f32).unwrap_or(0.0);
        out.push(Detection {
            bbox: Some(BoundingBox {
                x: bf("x"),
                y: bf("y"),
                width: bf("width"),
                height: bf("height"),
            }),
            class_label: map_get(dm, "class_label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            confidence: map_get(dm, "confidence").and_then(num_f32).unwrap_or(0.0),
            track_id: map_get(dm, "track_id").and_then(|v| v.as_u64()),
            assoc_confidence: map_get(dm, "assoc_confidence").and_then(num_f32),
            lock_state: map_get(dm, "lock_state")
                .and_then(|v| v.as_str())
                .and_then(parse_lock),
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        });
    }
    Ok(out)
}
