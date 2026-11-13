// Stream quality: how this window holds the drone on its `hero` video profile
// (`/api/video/profile`). A WFB drone boots on `thumbnail` (320x180 at 1 fps)
// until something promotes it, so the cockpit never relies on the ground
// station's fleet assignment to see a usable picture:
// - on every quality, Auto included, the Feed holds a hero lease for as long
//   as it is mounted and the page is visible (`useFeedHeroHold`, holder
//   `<lease owner>.feed`), released a few seconds after the Feed goes away;
// - High also pins a lease under the window's own lease owner for the life of
//   the page, so the drone stays on hero while this window shows another
//   screen. It is what `streamQualityOf` reads back: High while the node lists
//   that pinned lease.
// Both are shared hero holds (`@/shared/hero-hold`): renewed at a third of the
// lease, a failed renewal retried within seconds, a node without the route (a
// ground station) left alone. Demo mode takes neither.

import { create } from "zustand";

import { LEASE_OWNER } from "@/lib/lease-owner";
import { isDemoMode } from "@/lib/utils";
import { ApiError, apiFetch, errorDetail } from "@/shared/api-fetch";
import { toast } from "@/shared/api/toast";
import { createHeroHold, useHeroHold, VIDEO_PROFILE_PATH, type VideoProfileState } from "@/shared/hero-hold";

export const STREAM_QUALITIES = ["auto", "high"] as const;
export type StreamQuality = (typeof STREAM_QUALITIES)[number];

/** The Feed's own holder: distinct from the pinned lease, so leaving the Feed
 *  never drops a High choice. */
export const FEED_HOLDER = `${LEASE_OWNER}.feed`;

const feedHold = createHeroHold(FEED_HOLDER);
const pinHold = createHeroHold(LEASE_OWNER, { graceMs: 0 });
let releasePin: (() => void) | null = null;

/** The Feed holds the drone on hero while it is mounted and shown. */
export function useFeedHeroHold(): void {
  useHeroHold(feedHold, !isDemoMode());
}

/** The quality this window has chosen, read from the profile state: High
 *  while the node lists its pinned lease. */
export function streamQualityOf(state: VideoProfileState, holder: string): StreamQuality {
  return state.leases?.some((l) => l.holder === holder) ? "high" : "auto";
}

function unpin() {
  releasePin?.();
  releasePin = null;
}

/** The stream quality field's state: null until known, and while the node has
 *  no profile route. */
export const useStreamQualityStore = create<{
  quality: StreamQuality | null;
  load: () => Promise<void>;
  set: (q: StreamQuality) => Promise<void>;
}>((set, get) => ({
  quality: null,
  load: async () => {
    if (isDemoMode()) {
      set({ quality: "auto" });
      return;
    }
    let state: VideoProfileState;
    try {
      state = await apiFetch<VideoProfileState>(VIDEO_PROFILE_PATH);
    } catch {
      set({ quality: null });
      return;
    }
    // The node lists this window's pinned lease: keep it renewed, unless Auto
    // was chosen since (a release that has not landed yet).
    if (streamQualityOf(state, LEASE_OWNER) === "high" && !releasePin && get().quality !== "auto") releasePin = pinHold.acquire();
    set({ quality: releasePin ? "high" : "auto" });
  },
  set: async (q) => {
    if (isDemoMode()) {
      set({ quality: q });
      return;
    }
    if (q === "auto") {
      unpin();
      set({ quality: "auto" });
      return;
    }
    releasePin ??= pinHold.acquire();
    try {
      const quality = streamQualityOf((await pinHold.answer()) ?? {}, LEASE_OWNER);
      if (quality === "auto") unpin();
      set({ quality });
    } catch (e) {
      unpin();
      toast.err(e instanceof ApiError ? errorDetail(e.body, e.message) : String(e));
    }
  },
}));
