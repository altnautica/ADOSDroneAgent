import { mediaAuthHeaders, withMediaAuth } from "./media-auth";
// Lazy HLS player. iOS / macOS Safari can play HLS natively via the
// `<video>` element's `src` attribute. Chrome / Firefox / Edge need
// hls.js as a Media Source Extensions adapter. We dynamic-import
// hls.js so the ~110kB chunk only loads when the WebRTC path has
// failed and we're falling back.
//
// This is the cockpit twin of the dashboard's `startHls` (see
// dashboard/src/lib/hls.ts): the two SPAs are independent builds, so
// the helper is mirrored here rather than shared. Keep them in step.

export interface HlsSession {
  close: () => void;
}

export interface HlsResult {
  ok: boolean;
  session?: HlsSession;
  error?: string;
}

/**
 * Start HLS playback of `hlsUrl` into `videoEl`. `onLost` fires once if an
 * established session later dies beyond hls.js's own recovery, so the caller
 * stops calling a dead stream live and retries.
 */
export async function startHls(
  hlsUrl: string,
  videoEl: HTMLVideoElement,
  onLost?: () => void,
): Promise<HlsResult> {
  // Native HLS path (Safari, iOS, some Smart TVs). The element fetches the
  // playlist and segments itself and cannot send a header, so the session
  // rides the query string the agent accepts on the media plane only.
  if (videoEl.canPlayType("application/vnd.apple.mpegurl")) {
    videoEl.src = withMediaAuth(hlsUrl);
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
    // hls.js fetches the playlist AND every segment itself, so the credential
    // has to go on its own requests rather than on one call we make. Without
    // this hook there is no place for it at all, which is why the video paths
    // could not be gated before now.
    xhrSetup: (xhr: XMLHttpRequest) => {
      for (const [k, v] of Object.entries(mediaAuthHeaders())) {
        xhr.setRequestHeader(k, v);
      }
    },
    enableWorker: true,
    lowLatencyMode: false,
    backBufferLength: 30,
    maxBufferLength: 30,
    liveSyncDuration: 4,
    liveMaxLatencyDuration: 15,
  });

  return new Promise<HlsResult>((resolve) => {
    let resolved = false;
    let manifestTimer: ReturnType<typeof setTimeout> | null = null;
    const cleanup = () => {
      try {
        hls.destroy();
      } catch {
        /* noop */
      }
      videoEl.removeAttribute("src");
    };
    const settle = (result: HlsResult) => {
      if (manifestTimer) clearTimeout(manifestTimer);
      manifestTimer = null;
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
      // a fatal error to the layer. NETWORK_ERROR covers transient
      // playlist / segment fetch hiccups; MEDIA_ERROR covers the
      // BUFFER_STALLED + decode-side resets. Only when neither
      // recovery works do we fall through to close + settle so the
      // layer can show the error and retry.
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
      if (resolved) {
        // The session was established and has now died: tell the caller.
        onLost?.();
        return;
      }
      settle({
        ok: false,
        error: `HLS error: ${data.details ?? data.type}`,
      });
    });

    hls.attachMedia(videoEl);
    hls.loadSource(hlsUrl);

    // Safety net: 8s without a manifest = give up so the layer can retry.
    // Tear the player down first: a live hls.js left behind keeps fetching
    // segments and claims the <video> the next attempt plays into.
    manifestTimer = setTimeout(() => {
      manifestTimer = null;
      if (resolved) return;
      cleanup();
      settle({ ok: false, error: "HLS manifest timeout (8s)." });
    }, 8000);
  });
}
