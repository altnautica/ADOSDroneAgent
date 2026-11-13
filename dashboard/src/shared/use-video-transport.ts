// React binding for the shared video policy: one transport per mounted
// `<video>`, re-created when the leg URLs, the order or `reconnectKey` change.

import { useEffect, useRef, useState, type RefObject } from "react";

import type { VideoStats } from "./video-stats";
import {
  createVideoTransport,
  type TransportKind,
  type VideoTransportSnapshot,
} from "./video-transport";

export interface UseVideoTransportOptions {
  order: TransportKind[];
  whepUrl: string;
  hlsUrl: string;
  /** Changing this tears the session down and dials again (manual refresh). */
  reconnectKey?: string;
  /** False keeps the element idle (e.g. a panel scrolled out of view). */
  enabled?: boolean;
  noFrameMs?: number;
  /** The receive rates once a second (a debug readout); null when stopped.
   *  Changing it does not re-dial. */
  onStats?: (stats: VideoStats | null) => void;
}

const IDLE: VideoTransportSnapshot = {
  state: "connecting",
  transport: null,
  highLatency: false,
  error: null,
  width: null,
  height: null,
};

export function useVideoTransport(
  videoRef: RefObject<HTMLVideoElement | null>,
  { order, whepUrl, hlsUrl, reconnectKey, enabled = true, noFrameMs, onStats }: UseVideoTransportOptions,
): VideoTransportSnapshot {
  const [snapshot, setSnapshot] = useState<VideoTransportSnapshot>(IDLE);
  const orderKey = order.join(",");
  const statsRef = useRef(onStats);
  useEffect(() => {
    statsRef.current = onStats;
  }, [onStats]);

  useEffect(() => {
    const video = videoRef.current;
    if (!enabled || !video) return;
    const transport = createVideoTransport({
      order: orderKey.split(",").filter(Boolean) as TransportKind[],
      whepUrl,
      hlsUrl,
      video,
      noFrameMs,
      onChange: setSnapshot,
      onStats: (stats) => statsRef.current?.(stats),
    });
    transport.start();
    return () => {
      transport.stop();
      setSnapshot(IDLE);
    };
  }, [videoRef, orderKey, whepUrl, hlsUrl, reconnectKey, enabled, noFrameMs]);

  return snapshot;
}
