//! Encoder/publish split for in-process SEI injection.

use super::tap_splice::ffmpeg_output_stage;

/// The encoder→publish split the in-process SEI injector sits in.
///
/// `encode` writes raw Annex-B H.264 to stdout; `publish` reads it from stdin
/// and publishes it. The orchestrator runs both and pumps the bytes between
/// them through [`crate::sei::SeiInjector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeiSplit {
    pub encode: Vec<String>,
    pub publish: Vec<String>,
}

/// Separator token [`SeiSplit::render`] places between the two stages.
pub const SEI_STAGE_TOKEN: &str = "|sei|";

impl SeiSplit {
    /// Both stages as one token list, joined by [`SEI_STAGE_TOKEN`]. Used to
    /// read facts off the whole pipeline (encoder label, radio fan-out) and to
    /// pin the split in fixtures.
    pub fn render(&self) -> Vec<String> {
        let mut out = self.encode.clone();
        out.push(SEI_STAGE_TOKEN.to_string());
        out.extend(self.publish.iter().cloned());
        out
    }
}

/// Split the encoder→publish pipeline for the SEI injector.
///
/// The injector writes a wall-clock SEI NAL in front of every VCL slice so any
/// downstream consumer sees the same timestamp on the same frame (this is what
/// makes browser glass-to-glass measurement possible). Three cases:
/// - bash pipeline (rpicam path): the stage before the final `|` encodes, the
///   final stage publishes;
/// - raw ffmpeg → RTSP/UDP/TCP: split into encode-to-stdout and
///   publish-from-stdin, rebuilding the publisher for the original URI;
/// - GStreamer / unknown: `None`, the command runs unchanged.
pub fn wrap_with_sei_inject(cmd: &[String], output_uri: &str) -> Option<SeiSplit> {
    // Case 1: rpicam path is already a bash pipeline.
    if cmd.len() >= 3 && cmd[0] == "bash" && cmd[1] == "-c" {
        // The pipeline ends `... | ffmpeg ... <output>`. Split before the final
        // stage (rsplit on '|' with maxsplit=1).
        let (head, tail) = cmd[2].rsplit_once('|')?;
        return Some(SeiSplit {
            encode: vec!["bash".into(), "-c".into(), head.trim_end().to_string()],
            publish: vec!["bash".into(), "-c".into(), tail.trim_start().to_string()],
        });
    }
    // Case 3: gstreamer or unknown — run unchanged.
    if cmd.first().map(String::as_str) != Some("ffmpeg") {
        return None;
    }
    // Case 2: raw ffmpeg cmd publishing to RTSP/UDP/TCP. Split into two stages.
    if !(output_uri.starts_with("rtsp://")
        || output_uri.starts_with("udp://")
        || output_uri.starts_with("tcp://"))
    {
        // Unknown output URI — cannot rebuild the publisher.
        return None;
    }
    // Did the builder append the radio fan-out? Then the output stage ends in
    // the tee spec rather than the bare URI, and the rebuilt publish stage has
    // to carry the same two branches — otherwise turning the SEI probe on
    // would silently take the radio leg down.
    let fanout = cmd
        .last()
        .is_some_and(|t| t.starts_with("[f=") && t.contains(output_uri));
    let mut encoded: Vec<String> = cmd.to_vec();
    // Strip the destination: the output URI, or the tee spec that carries it
    // (both are the last token).
    if fanout || encoded.last().map(String::as_str) == Some(output_uri) {
        encoded.pop();
    }
    // Strip the output stage's muxer selection + flags, right-to-left, so the
    // INPUT side's `-f v4l2` / `-fflags` are never touched.
    strip_flag_with_value(&mut encoded, "-f");
    if fanout {
        strip_flag_with_value(&mut encoded, "-map");
    }
    strip_flag_with_value(&mut encoded, "-rtsp_transport");
    strip_flag_with_value(&mut encoded, "-muxpreload");
    strip_flag_with_value(&mut encoded, "-muxdelay");
    strip_flag_with_value(&mut encoded, "-max_delay");
    // Encode-only ffmpeg now emits raw Annex-B H.264 on stdout.
    encoded.push("-f".into());
    encoded.push("h264".into());
    encoded.push("-".into());

    // Publish-only ffmpeg pulls the SEI-stamped Annex-B from stdin and
    // re-mounts exactly the destination(s) the builder had chosen, through the
    // SAME output-stage builder — so the mux-delay strip and the radio fan-out
    // cannot be present on one path and missing on the other.
    let mut publish: Vec<String> = [
        "ffmpeg",
        "-loglevel",
        "error",
        "-fflags",
        "nobuffer",
        "-flags",
        "low_delay",
        "-f",
        "h264",
        "-i",
        "-",
        "-c",
        "copy",
        "-flush_packets",
        "1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    publish.extend(ffmpeg_output_stage(output_uri, fanout));
    Some(SeiSplit {
        encode: encoded,
        publish,
    })
}

/// Remove the last occurrence of `flag` and its following value from `args`,
/// scanning right-to-left, and return the value.
pub(super) fn take_flag_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    if args.len() < 2 {
        return None;
    }
    // i runs from len-1 down to 1; act when args[i]==flag
    // and a value follows (i+1 < len). Take the highest such i (right-most).
    for i in (1..args.len()).rev() {
        if args[i] == flag && i + 1 < args.len() {
            let value = args.remove(i + 1);
            args.remove(i);
            return Some(value);
        }
    }
    None
}

/// Remove the last occurrence of `flag` and its following value from `args`,
/// discarding the value.
pub(super) fn strip_flag_with_value(args: &mut Vec<String>, flag: &str) {
    let _ = take_flag_value(args, flag);
}
