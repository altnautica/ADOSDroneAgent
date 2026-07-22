// Feed-local UI state shared across the immersive surface: which camera the
// multi-stream tabs have selected, a nonce the Stream action bumps to force the
// video layer to re-establish its WHEP session (a manual "refresh the feed" for
// when a stream freezes), and the live video-layer connection state + decoded
// resolution the status strip's video zone reads (so "which feed am I looking
// at, is it live, at what resolution" is answered from the real <video> element,
// never fabricated). Kept out of the navigator store because it is screen-local,
// not navigation.

import { create } from "zustand";

/** The WHEP video-layer connection state, mirrored from the video layer. */
export type VideoState = "connecting" | "live" | "error";

interface FeedState {
  /** The selected camera id (from the roster), or null for the primary leg. */
  activeCameraId: string | null;
  /** Bumped to force the video layer to reconnect its WHEP session. */
  streamNonce: number;
  /** The label of the currently-selected stream, for the strip's video zone. */
  activeStreamLabel: string | null;
  /** The live WHEP connection state of the video layer. */
  videoState: VideoState;
  /** The decoded resolution of the playing feed, read off the <video> element. */
  videoWidth: number | null;
  videoHeight: number | null;

  setActiveCamera: (id: string | null) => void;
  reconnectStream: () => void;
  setActiveStreamLabel: (label: string | null) => void;
  setVideoStatus: (state: VideoState, width: number | null, height: number | null) => void;
}

export const useFeedStore = create<FeedState>((set) => ({
  activeCameraId: null,
  streamNonce: 0,
  activeStreamLabel: null,
  videoState: "connecting",
  videoWidth: null,
  videoHeight: null,
  setActiveCamera: (id) => set({ activeCameraId: id }),
  reconnectStream: () => set((s) => ({ streamNonce: s.streamNonce + 1 })),
  setActiveStreamLabel: (label) => set({ activeStreamLabel: label }),
  setVideoStatus: (videoState, videoWidth, videoHeight) =>
    set({ videoState, videoWidth, videoHeight }),
}));
