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

  const hls = new HlsCtor({
    enableWorker: true,
    lowLatencyMode: true,
    backBufferLength: 30,
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
      if (data.fatal) {
        cleanup();
        settle({
          ok: false,
          error: `HLS error: ${data.details ?? data.type}`,
        });
      }
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
