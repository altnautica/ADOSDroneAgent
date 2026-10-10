// HLS playback. Safari/iOS play HLS natively through the element's `src`;
// Chrome/Firefox/Edge need hls.js as a Media Source Extensions adapter. The
// light build (no subtitles, alternate audio or DRM, none of which this stream
// carries) is imported lazily so it loads only when HLS is actually needed.
//
// One player per element: starting a new session on an element tears down any
// player still attached to it, so a retry can never leave a second instance
// fetching segments into the same `<video>`.

import { credentialHeaders } from "./api-fetch";
import { withMediaAuth } from "./media-auth";

export interface HlsSession {
  close: () => void;
}

export interface HlsResult {
  ok: boolean;
  session?: HlsSession;
  error?: string;
}

/** The subset of the hls.js API this module drives (a test seam). */
export interface HlsPlayer {
  on(event: string, cb: (evt: string, data: HlsErrorData) => void): void;
  attachMedia(el: HTMLVideoElement): void;
  loadSource(url: string): void;
  startLoad(): void;
  recoverMediaError(): void;
  destroy(): void;
}

export interface HlsErrorData {
  fatal?: boolean;
  type?: string;
  details?: string;
}

export interface HlsCtor {
  new (config: Record<string, unknown>): HlsPlayer;
  isSupported(): boolean;
  Events: { MANIFEST_PARSED: string; ERROR: string; FRAG_LOADED: string };
  ErrorTypes: { NETWORK_ERROR: string; MEDIA_ERROR: string };
}

/** Consecutive fatal network errors after which an established session is
 *  declared lost (hls.js recovery is retried before that). */
export const HLS_MAX_FATAL_NETWORK_ERRORS = 3;
const MANIFEST_TIMEOUT_MS = 8000;
// Lazy on purpose: the player is a separate chunk that only loads when a feed
// actually falls back to HLS, keeping it out of the main bundle budget.
const loadLightBuild = async (): Promise<HlsCtor> =>
  (await import("hls.js/dist/hls.light.mjs")).default as unknown as HlsCtor;

const active = new WeakMap<HTMLVideoElement, () => void>();

export async function startHls(
  hlsUrl: string,
  videoEl: HTMLVideoElement,
  onLost?: () => void,
  loadHls: () => Promise<HlsCtor> = loadLightBuild,
): Promise<HlsResult> {
  active.get(videoEl)?.();
  active.delete(videoEl);

  // Native path: the element fetches playlist and segments itself and cannot
  // send a header, so the session rides the media-plane query parameter.
  if (videoEl.canPlayType("application/vnd.apple.mpegurl")) {
    videoEl.src = withMediaAuth(hlsUrl);
    videoEl.play().catch(() => undefined);
    const close = () => {
      active.delete(videoEl);
      videoEl.pause();
      videoEl.removeAttribute("src");
      videoEl.load();
    };
    active.set(videoEl, close);
    return { ok: true, session: { close } };
  }

  let Hls: HlsCtor;
  try {
    Hls = await loadHls();
  } catch (err) {
    return {
      ok: false,
      error: `hls.js failed to load: ${err instanceof Error ? err.message : String(err)}`,
    };
  }
  if (!Hls.isSupported()) {
    return { ok: false, error: "Browser supports neither native HLS nor MSE." };
  }

  // Standard HLS, not LL-HLS: the media server serves fMP4 without parts, and
  // low-latency mode would send blocking-reload queries it does not honour.
  const hls = new Hls({
    xhrSetup: (xhr: XMLHttpRequest) => {
      for (const [k, v] of Object.entries(credentialHeaders())) xhr.setRequestHeader(k, v);
    },
    enableWorker: true,
    lowLatencyMode: false,
    backBufferLength: 30,
    maxBufferLength: 30,
    liveSyncDuration: 4,
    liveMaxLatencyDuration: 15,
  });

  return new Promise<HlsResult>((resolve) => {
    let settled = false;
    let destroyed = false;
    let networkErrors = 0;

    const destroy = () => {
      if (destroyed) return;
      destroyed = true;
      clearTimeout(manifestTimer);
      if (active.get(videoEl) === destroy) active.delete(videoEl);
      try {
        hls.destroy();
      } catch {
        /* already gone */
      }
      videoEl.removeAttribute("src");
    };
    active.set(videoEl, destroy);

    const fail = (error: string) => {
      destroy();
      if (settled) {
        onLost?.();
      } else {
        settled = true;
        resolve({ ok: false, error });
      }
    };

    hls.on(Hls.Events.MANIFEST_PARSED, () => {
      clearTimeout(manifestTimer);
      videoEl.play().catch(() => undefined);
      if (!settled) {
        settled = true;
        resolve({ ok: true, session: { close: destroy } });
      }
    });

    // A segment arrived: the network path works again.
    hls.on(Hls.Events.FRAG_LOADED, () => {
      networkErrors = 0;
    });

    hls.on(Hls.Events.ERROR, (_evt, data) => {
      if (!data.fatal || destroyed) return;
      if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
        networkErrors += 1;
        if (networkErrors < HLS_MAX_FATAL_NETWORK_ERRORS) {
          hls.startLoad();
          return;
        }
      } else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
        hls.recoverMediaError();
        return;
      }
      fail(`HLS error: ${data.details ?? data.type ?? "fatal"}`);
    });

    hls.attachMedia(videoEl);
    hls.loadSource(hlsUrl);

    const manifestTimer = setTimeout(() => {
      if (!settled) fail(`HLS manifest timeout (${MANIFEST_TIMEOUT_MS / 1000}s).`);
    }, MANIFEST_TIMEOUT_MS);
  });
}
