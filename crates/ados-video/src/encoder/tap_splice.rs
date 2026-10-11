//! Shared `ffmpeg` publish stage, including the `tee` RTP fan-out to the radio.

/// The output half of an `ffmpeg` publish argv: the muxer flags, the muxer
/// selection, and the destination(s).
///
/// Shared by [`build_ffmpeg_command`] and the SEI-spliced publish stage in
/// [`wrap_with_sei_inject`] so the two cannot drift — a publish stage that
/// differs between "SEI off" and "SEI on" is how a latency flag gets fixed on
/// one path only.
///
/// `rtp_fanout` (RTSP destinations only) replaces the single RTSP output with
/// ffmpeg's `tee` muxer: ONE encode, two muxers, the second emitting the
/// radio's RTP copy directly. That is what removes the whole extra `ffmpeg`
/// that used to re-read the published stream back out of mediamtx.
pub(super) fn ffmpeg_output_stage(output: &str, rtp_fanout: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if output.starts_with("rtsp://") {
        // `-muxdelay 0 -muxpreload 0` strip ffmpeg's default ~0.7 s mux delay
        // and ~0.5 s preload. Every other ffmpeg in this tree sets them and
        // says so; the primary encoder — the one a USB or IP camera uses, i.e.
        // the default path — did not, which left up to 1.2 s of avoidable
        // latency in front of every downstream hop.
        //
        // They are ffmpeg CLI *output* options, NOT `AVFormatContext`
        // options, so they cannot be passed per-branch inside a tee spec:
        // ffmpeg answers `Unknown option 'muxdelay'` and the tee aborts the
        // whole output (verified on ffmpeg 9.0.1). They belong here, ahead of
        // the muxer selection, on both the plain and the tee form.
        out.extend(
            ["-max_delay", "0", "-muxdelay", "0", "-muxpreload", "0"]
                .iter()
                .map(|s| s.to_string()),
        );
        if rtp_fanout {
            // The tee muxer needs an explicit `-map`; without one ffmpeg
            // refuses to build the output.
            out.push("-map".into());
            out.push("0:v".into());
            out.push("-f".into());
            out.push("tee".into());
            out.push(tee_spec(output));
        } else {
            // TCP RTSP avoids UDP fragmentation of large keyframe NALs.
            out.extend(
                ["-rtsp_transport", "tcp", "-f", "rtsp"]
                    .iter()
                    .map(|s| s.to_string()),
            );
            out.push(output.to_string());
        }
    } else if output.starts_with("udp://") || output.starts_with("tcp://") {
        // Same mux-delay strip as the RTSP branch: the MPEG-TS muxer inherits
        // the same ffmpeg defaults, and this sink is a live feed too.
        out.extend(
            ["-muxdelay", "0", "-muxpreload", "0", "-f", "mpegts"]
                .iter()
                .map(|s| s.to_string()),
        );
        out.push(output.to_string());
    } else {
        out.push(output.to_string());
    }
    out
}

/// The `-f tee` branch specification: the RTSP publish into mediamtx, plus the
/// wfb radio's RTP copy on UDP 5600.
///
/// Both branches carry the same already-encoded packets, so the RTP copy costs
/// no second encode and no re-read. Framing is RTP (RFC 6184) because that is
/// wfb-ng's contract: each datagram must survive single-packet loss on its own,
/// and raw H.264 over UDP corrupts silently to the next start code. The payload
/// type, SSRC, packet size and destination all come from [`crate::wfb_tee`], so
/// the receiver's static SDP keeps describing the stream exactly.
///
/// Deliberately NO `onfail=ignore`: if the radio branch cannot be opened, this
/// encoder MUST fail and be restarted by the supervisor rather than quietly
/// serving LAN video with a dead radio leg. That shared fate is what replaces
/// the retired tap's own progress watchdog — one encode, one process, one
/// liveness signal, with the bytes-on-air counter (`wfb_tx`) as the
/// independent downstream check.
///
/// Per-branch options are `AVFormatContext` options only (`max_delay`,
/// `flush_packets`, `rtsp_transport`, `payload_type`, `ssrc`); the CLI-level
/// mux-delay strip is emitted by [`ffmpeg_output_stage`].
pub(super) fn tee_spec(rtsp_output: &str) -> String {
    format!(
        "[f=rtsp:rtsp_transport=tcp:max_delay=0:flush_packets=1]{rtsp_output}\
         |[f=rtp:payload_type={pt}:ssrc={ssrc}:max_delay=0:flush_packets=1]{rtp}",
        pt = crate::wfb_tee::WFB_TEE_PAYLOAD_TYPE,
        ssrc = crate::wfb_tee::WFB_TEE_SSRC,
        rtp = crate::wfb_tee::rtp_destination_url(),
    )
}
