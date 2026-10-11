//! Runtime environment probes the encoder builder consumes.

use ados_protocol::hwcaps::{EncoderDevice, Probed};

/// The resolved runtime environment the builder needs. Gathering these probes
/// up front keeps [`build_encoder_command`] pure and unit-testable.
#[derive(Debug, Clone)]
pub struct EncoderEnv {
    /// The probed hardware H.264 encoder device. `Present` only when a real
    /// V4L2 node enumerates an H.264 output FourCC AND accepted a bounded
    /// trial-init; the builder then selects `h264_v4l2m2m`. `Absent`
    /// (or `NotProbed`) selects software `libx264`. This is a *probe*, not a
    /// trust of ffmpeg's `-encoders` listing.
    pub hw_h264: Probed<EncoderDevice>,
    /// GStreamer `mpph264enc` (Rockchip VPU) is installed.
    pub has_mpph264enc: bool,
    /// GStreamer `omxh264videoenc` (Allwinner Cedar OMX HW encoder) is installed.
    /// Its presence is the honest Allwinner vendor gate: the element only ships
    /// on a board with the Cedar video engine, so probing it IS how we learn the
    /// board's HAL `encoder_api == "vendor"` from Rust (there is no Rust-side
    /// board-YAML reader; the element is the ground truth).
    pub has_omxh264videoenc: bool,
    /// GStreamer `rtspclientsink` element is installed (direct RTSP RECORD;
    /// otherwise the gstreamer RTSP path pipes through ffmpeg).
    pub has_rtspclientsink: bool,
    /// The board's HAL `video.encoder_api` capability: "vendor" (Allwinner
    /// OMX/Cedar), "rkmpp"/"mpp" (Rockchip VPU), "v4l2", "rkmedia", "none",
    /// "unknown". Derived at probe time from which HW GStreamer element is
    /// present (see [`EncoderEnv::detect`]); consumed to pick the OMX branch.
    pub encoder_api: String,
    /// The board is Pi-5-class silicon (BCM2712 — Raspberry Pi 5 / CM5).
    ///
    /// Probed from the kernel's own device-tree `compatible` list via
    /// [`ados_hal_probe::probe_soc`] and classified by [`soc_is_pi5_class`].
    /// This family has **no** hardware H.264 encoder (VideoCore VII dropped
    /// the encode block that VideoCore VI on the Pi 4 / CM4 had), so
    /// `rpicam-vid` there is a software x264 encoder and must be given
    /// `--low-latency` or it buffers a constant 8-frame pipeline.
    pub pi5_class: bool,
    /// Usable CPU parallelism, as the kernel reports it
    /// ([`std::thread::available_parallelism`]).
    ///
    /// This is an encoder LATENCY input, not a throughput one. The software
    /// x264 path is the only encode path left on Pi-5-class silicon (BCM2712
    /// dropped the encode block), and its pipeline delay is set by how the
    /// frame is threaded: frame-level threading holds `threads` frames in
    /// flight before the first slice comes out, while sliced threading splits
    /// ONE frame across the cores and emits it whole. `-tune zerolatency`
    /// already asks for the latter; the builder used to override it back off.
    /// Emitting the sliced-threads levers needs real cores to split across, so
    /// the count is probed rather than assumed: on a 1-2 core board the levers
    /// would oversubscribe and cost more than they save.
    pub cpu_threads: u32,
}

impl EncoderEnv {
    /// Probe the real environment. On non-Linux hosts the probes are no-ops
    /// so the builder is exercisable on the dev host; the rig path is Linux.
    ///
    /// This blocks: it shells `gst-inspect-1.0` up to three times and
    /// trial-inits a V4L2 encoder device, and `gst-inspect` on a cold SBC page
    /// cache is not fast. Async callers MUST use [`EncoderEnv::detect_async`]
    /// instead; this entry point stays for the synchronous callers and tests.
    #[cfg(target_os = "linux")]
    pub fn detect() -> Self {
        // Probe for a REAL hardware H.264 encoder device rather than trusting
        // the ffmpeg `-encoders` listing. Runs at the pre-arm boot phase: the
        // trial-init opens and configures a device, so it is never run during
        // armed runtime (the probe defers to its cached result there).
        let hw_h264 = ados_hal_probe::probe::video::probe_h264_encoder(
            ados_protocol::hwcaps::ProbePhase::BootPreArm,
        );

        // Board family, from the same probe-first HAL: read-only, phase-free,
        // and the kernel's own answer rather than a board-YAML declaration.
        let pi5_class = ados_hal_probe::probe_soc()
            .value()
            .is_some_and(|soc| soc_is_pi5_class(&soc.0));

        let has_mpph264enc = gst_element_present("mpph264enc");
        let has_omxh264videoenc = gst_element_present("omxh264videoenc");
        let encoder_api = if has_omxh264videoenc {
            "vendor".to_string()
        } else if has_mpph264enc {
            "rkmpp".to_string()
        } else {
            "unknown".to_string()
        };
        Self {
            hw_h264,
            has_mpph264enc,
            has_omxh264videoenc,
            has_rtspclientsink: gst_element_present("rtspclientsink"),
            encoder_api,
            pi5_class,
            cpu_threads: detect_cpu_threads(),
        }
    }

    /// Non-Linux fallback: software libx264 everywhere, no GStreamer HW.
    #[cfg(not(target_os = "linux"))]
    pub fn detect() -> Self {
        Self {
            hw_h264: Probed::NotProbed,
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: false,
            encoder_api: "unknown".to_string(),
            pi5_class: false,
            cpu_threads: detect_cpu_threads(),
        }
    }

    /// [`EncoderEnv::detect`] moved off the async runtime.
    ///
    /// The probes are blocking subprocess spawns and device ioctls; running
    /// them inline on a tokio worker stalls every other task on that worker
    /// for as long as `gst-inspect-1.0` takes to fault its plugin registry in.
    /// Async callers must use this. The closure is the unchanged sync body, so
    /// there is exactly one probe implementation.
    pub async fn detect_async() -> Self {
        tokio::task::spawn_blocking(Self::detect)
            .await
            // `detect` cannot panic (every probe is fallible-and-defaulted), so
            // a JoinError here can only mean runtime shutdown; the honest answer
            // then is the same all-absent env the non-Linux path returns.
            .unwrap_or_else(|_| Self {
                hw_h264: Probed::NotProbed,
                has_mpph264enc: false,
                has_omxh264videoenc: false,
                has_rtspclientsink: false,
                encoder_api: "unknown".to_string(),
                pi5_class: false,
                cpu_threads: detect_cpu_threads(),
            })
    }
}

#[cfg(target_os = "linux")]
pub(super) fn gst_element_present(element: &str) -> bool {
    std::process::Command::new("gst-inspect-1.0")
        .arg(element)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Usable CPU parallelism, or 1 when the kernel will not say.
///
/// `available_parallelism` honours the cgroup CPU quota and the affinity mask,
/// which is what this decision needs: a video unit pinned to two cores by a
/// `CPUAffinity=` must not be handed four encode threads. Falling back to 1
/// (not to a guess of 4) keeps the conservative argv on an unknown host — the
/// sliced-threads levers are only emitted where the cores are proven present.
pub(super) fn detect_cpu_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// True when a device-tree `compatible` list names Pi-5-class silicon.
///
/// The list is what `/proc/device-tree/compatible` exposes, most-specific
/// first: `raspberrypi,5-model-b` + `brcm,bcm2712` on a Pi 5,
/// `raspberrypi,5-compute-module` + `brcm,bcm2712` on a CM5. Either the SoC
/// entry or the board entry is enough, so a downstream device tree that omits
/// one of them still classifies correctly.
///
/// The family matters to the builder because it is the boundary of the
/// hardware H.264 encoder: BCM2711 (Pi 4 / CM4, VideoCore VI) has one and
/// BCM2712 (Pi 5 / CM5, VideoCore VII) does not, so on Pi-5-class boards
/// `rpicam-vid` is a software encoder and needs `--low-latency`.
pub fn soc_is_pi5_class(compatibles: &[String]) -> bool {
    compatibles.iter().any(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("bcm2712") || c.contains("raspberrypi,5")
    })
}
