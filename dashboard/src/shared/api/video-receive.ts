// The ground station's video receive state, shared by the cockpit Radio link
// screen and the dashboard Radio link page: the stream state, codec tracks and
// viewer count from `GET /api/video` (the station's media server) and the
// glass-to-glass latency the SEI probe measures from `GET /api/video/latency`.
// A value the agent does not report reads as a dash.

import { DASH } from "../format";

export const VIDEO_STATUS_PATH = "/api/video";
export const VIDEO_LATENCY_PATH = "/api/video/latency";

export type VideoReceiveTone = "ok" | "warn" | "muted";

export interface VideoStatusWire {
  state?: string | null;
  mediamtx?: { running?: boolean; ready?: boolean; tracks?: unknown[]; readers?: number } | null;
}
export interface VideoLatencyWire {
  latency_ms?: number | null;
  samples?: number | null;
}

export interface VideoReceiveView {
  stateKey:
    | "cockpit.link.video.receiving"
    | "cockpit.link.video.noStream"
    | "cockpit.link.video.serverDown"
    | "cockpit.link.video.unknown";
  tone: VideoReceiveTone;
  tracks: string;
  viewers: string;
  latency: string;
}

/** Pure view over the two reads. */
export function videoReceiveView(
  video: VideoStatusWire | null | undefined,
  latency: VideoLatencyWire | null | undefined,
): VideoReceiveView {
  const mtx = video?.mediamtx ?? null;
  const stateKey =
    video == null
      ? "cockpit.link.video.unknown"
      : mtx?.running === false
        ? "cockpit.link.video.serverDown"
        : video.state === "running" && mtx?.ready
          ? "cockpit.link.video.receiving"
          : "cockpit.link.video.noStream";
  const tone: VideoReceiveTone =
    stateKey === "cockpit.link.video.receiving" ? "ok" : stateKey === "cockpit.link.video.unknown" ? "muted" : "warn";
  const tracks = Array.isArray(mtx?.tracks) && mtx.tracks.length > 0 ? mtx.tracks.map(String).join(", ") : DASH;
  const viewers = typeof mtx?.readers === "number" ? String(mtx.readers) : DASH;
  const ms = latency?.latency_ms;
  return {
    stateKey,
    tone,
    tracks,
    viewers,
    latency: typeof ms === "number" && Number.isFinite(ms) ? `${Math.round(ms)} ms` : DASH,
  };
}
