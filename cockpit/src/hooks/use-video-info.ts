// Polls the video config/link snapshot (`GET /api/video/config`) every 10 s and
// derives an honest "what is flowing" read for the status strip's video zone.
// The rate is profile-aware so it never fabricates: on a drone (the video
// SOURCE) it is the drone's own encoder rate/fps; on a ground station (a
// receiver) it is the RF video byte rate the receiver actually measures
// (`link.video_inbound_bytes_per_s`) — the config `encoder` block on a receiver
// is that box's own unused camera default and must NOT be shown as the received
// stream. Anything not honestly known for the profile stays null so the strip
// dashes it.

import { useEffect, useState } from "react";

import { pollLoop } from "@/hooks/use-status-poll";

import type { AgentProfile } from "@/shared/use-profile";
import { getVideoConfig } from "@/lib/api";
import type { VideoConfigResponse } from "@/lib/types";

export interface VideoInfo {
  /** The video data rate in Mbps, honest per profile, or null when unknown. */
  rateMbps: number | null;
  /** Encoder fps — meaningful only on the source (a drone); null on a receiver. */
  fps: number | null;
}

/** Derive the honest rate/fps from a `/api/video/config` body for the profile.
 *  A drone reads its own encoder (it is the source); a receiver reads only the
 *  measured inbound video byte rate. */
export function deriveVideoInfo(
  cfg: VideoConfigResponse | null,
  profile: AgentProfile | null,
): VideoInfo {
  if (profile === "drone") {
    const kbps = typeof cfg?.encoder?.bitrate_kbps === "number" ? cfg.encoder.bitrate_kbps : null;
    const fps = typeof cfg?.encoder?.fps === "number" ? cfg.encoder.fps : null;
    return { rateMbps: kbps != null ? kbps / 1000 : null, fps };
  }
  const bps =
    typeof cfg?.link?.video_inbound_bytes_per_s === "number"
      ? cfg.link.video_inbound_bytes_per_s
      : null;
  return { rateMbps: bps != null && bps > 0 ? (bps * 8) / 1e6 : null, fps: null };
}

export function useVideoInfo(profile: AgentProfile | null, intervalMs = 10_000): VideoInfo {
  const [info, setInfo] = useState<VideoInfo>({ rateMbps: null, fps: null });

  useEffect(
    () =>
      pollLoop(
        async (signal) => {
          try {
            setInfo(deriveVideoInfo(await getVideoConfig(signal), profile));
          } catch {
            // Keep the last read: a non-critical slow poll.
          }
        },
        () => intervalMs,
      ),
    [profile, intervalMs],
  );

  return info;
}
