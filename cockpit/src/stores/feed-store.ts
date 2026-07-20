// Feed-local UI state shared across the immersive surface: which camera the
// multi-stream tabs have selected, and a nonce the Stream action bumps to force
// the video layer to re-establish its WHEP session (a manual "refresh the feed"
// for when a stream freezes). Kept out of the navigator store because it is
// screen-local, not navigation.

import { create } from "zustand";

interface FeedState {
  /** The selected camera id (from the roster), or null for the primary leg. */
  activeCameraId: string | null;
  /** Bumped to force the video layer to reconnect its WHEP session. */
  streamNonce: number;

  setActiveCamera: (id: string | null) => void;
  reconnectStream: () => void;
}

export const useFeedStore = create<FeedState>((set) => ({
  activeCameraId: null,
  streamNonce: 0,
  setActiveCamera: (id) => set({ activeCameraId: id }),
  reconnectStream: () => set((s) => ({ streamNonce: s.streamNonce + 1 })),
}));
