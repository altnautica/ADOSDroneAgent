// Lazy HLS player. iOS / macOS Safari can play HLS natively via the
// `<video>` element's `src` attribute. Chrome / Firefox / Edge need
// hls.js as a Media Source Extensions adapter. We dynamic-import
// hls.js so the ~110kB chunk only loads when the WebRTC path has
// failed and we're falling back.

export interface HlsSession {
  close: () => void;
}

export interface HlsResult {
  ok: boolean;
  session?: HlsSession;
  error?: string;
}

export async function startHls(
  hlsUrl: string,
  videoEl: HTMLVideoElement,
): Promise<HlsResult> {
  // Native HLS path (Safari, iOS, some Smart TVs)
  if (videoEl.canPlayType("application/vnd.apple.mpegurl")) {
    videoEl.src = hlsUrl;
    try {
      await videoEl.play();
    } catch {
      // Autoplay can be blocked; the user will see a play button.
    }
    return {
      ok: true,
      session: {
        close: () => {
          videoEl.pause();
          videoEl.removeAttribute("src");
          videoEl.load();
        },
      },
    };
  }

  // Chrome / Firefox / Edge — pull in hls.js dynamically
  let HlsCtor: typeof import("hls.js").default;
  try {
    const mod = await import("hls.js");
    HlsCtor = mod.default;
  } catch (err) {
    return {
      ok: false,
      error:
        "hls.js failed to load: " +
        (err instanceof Error ? err.message : String(err)),
    };
  }

  if (!HlsCtor.isSupported()) {
    return {
      ok: false,
      error: "Browser supports neither native HLS nor MSE.",
    };
  }

  // Standard HLS, not LL-HLS. MediaMTX serves fMP4 with no parts +
  // no blocking playlist reloads on this rig; turning on
  // lowLatencyMode here would send _HLS_msn=N&_HLS_part=M query
  // strings the server doesn't honor and use tighter buffer
  // thresholds that drain faster than they refill on 1.5 s segments
  // → player stalls and stops polling while MediaMTX is healthy.
  // maxBufferLength + liveMaxLatencyDuration give the player room
  // to absorb a brief network stutter without re-buffering, then
  // jump forward instead of falling perpetually behind live edge.
  const hls = new HlsCtor({
    enableWorker: true,
    lowLatencyMode: false,
    backBufferLength: 30,
    maxBufferLength: 30,
    liveSyncDuration: 4,
    liveMaxLatencyDuration: 15,
  });

  return new Promise<HlsResult>((resolve) => {
    let resolved = false;
    const cleanup = () => {
      try {
        hls.destroy();
      } catch {
        /* noop */
      }
      videoEl.removeAttribute("src");
    };
    const settle = (result: HlsResult) => {
      if (!resolved) {
        resolved = true;
        resolve(result);
      }
    };

    hls.on(HlsCtor.Events.MANIFEST_PARSED, () => {
      videoEl.play().catch(() => undefined);
      settle({
        ok: true,
        session: {
          close: cleanup,
        },
      });
    });

    hls.on(HlsCtor.Events.ERROR, (_evt, data) => {
      if (!data.fatal) return;
      // Recoverable cases: ask HLS.js to retry instead of surfacing
      // a fatal error to the panel. NETWORK_ERROR covers transient
      // playlist / segment fetch hiccups; MEDIA_ERROR covers the
      // BUFFER_STALLED + decode-side resets. Only when neither
      // recovery works do we fall through to close + settle so the
      // panel can show the error and try the next transport.
      switch (data.type) {
        case HlsCtor.ErrorTypes.NETWORK_ERROR:
          try {
            hls.startLoad();
            return;
          } catch {
            /* fall through to error */
          }
          break;
        case HlsCtor.ErrorTypes.MEDIA_ERROR:
          try {
            hls.recoverMediaError();
            return;
          } catch {
            /* fall through to error */
          }
          break;
      }
      cleanup();
      settle({
        ok: false,
        error: `HLS error: ${data.details ?? data.type}`,
      });
    });

    hls.attachMedia(videoEl);
    hls.loadSource(hlsUrl);

    // Safety net: 8s without a manifest = give up so the panel can
    // try the snapshot fallback.
    setTimeout(() => {
      settle({ ok: false, error: "HLS manifest timeout (8s)." });
    }, 8000);
  });
}
