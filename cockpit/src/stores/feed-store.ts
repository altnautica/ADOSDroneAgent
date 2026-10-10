// Feed-local UI state shared across the immersive surface: which camera the
// stream tabs selected, a nonce the Stream action bumps to force the video to
// re-dial, the camera switch in flight, and the live video snapshot (state,
// transport, decoded resolution) mirrored from the real `<video>` element so
// the status strip and alerts never fabricate what is on screen.

import { create } from "zustand";

import type { VideoTransportSnapshot } from "@/shared/video-transport";

const IDLE_VIDEO: VideoTransportSnapshot = {
  state: "connecting",
  transport: null,
  highLatency: false,
  error: null,
  width: null,
  height: null,
};

interface FeedState {
  /** The selected camera id, or null for the primary leg. */
  activeCameraId: string | null;
  /** Bumped to force the video layer to re-dial. */
  streamNonce: number;
  /** The label of the currently-selected stream. */
  activeStreamLabel: string | null;
  /** The live video snapshot from the shared transport. */
  video: VideoTransportSnapshot;
  /** True while the video layer is mounted (only on the Feed). */
  videoMounted: boolean;

  setActiveCamera: (id: string | null) => void;
  reconnectStream: () => void;
  setActiveStreamLabel: (label: string | null) => void;
  setVideo: (video: VideoTransportSnapshot | null) => void;
}

export const useFeedStore = create<FeedState>((set) => ({
  activeCameraId: null,
  streamNonce: 0,
  activeStreamLabel: null,
  video: IDLE_VIDEO,
  videoMounted: false,
  setActiveCamera: (id) => set({ activeCameraId: id }),
  reconnectStream: () => set((s) => ({ streamNonce: s.streamNonce + 1 })),
  setActiveStreamLabel: (label) => set({ activeStreamLabel: label }),
  setVideo: (video) =>
    set(video ? { video, videoMounted: true } : { video: IDLE_VIDEO, videoMounted: false }),
}));
