// Stream quality: the drone's attention profile (`/api/video/profile`). On
// Auto the drone streams at the profile the ground station assigned; on High
// this window holds a hero lease, renewed while it stays chosen, so the drone
// streams full rate for this pilot without moving the fleet's assignment. A
// node without the route (a ground station, no video pipeline) has no field.

import { create } from "zustand";

import { LEASE_OWNER } from "@/lib/lease-owner";
import { isDemoMode } from "@/lib/utils";
import { ApiError, apiFetch, errorDetail } from "@/shared/api-fetch";
import { toast } from "@/shared/api/toast";

export const VIDEO_PROFILE_PATH = "/api/video/profile";
export const STREAM_QUALITIES = ["auto", "high"] as const;
export type StreamQuality = (typeof STREAM_QUALITIES)[number];
/** The hero lease asked for, and how often it is renewed (well inside it). */
export const QUALITY_LEASE_S = 30;
export const QUALITY_RENEW_MS = 10_000;

interface VideoProfileState {
  leases?: { holder: string }[];
}

/** The quality this window has chosen, read from the profile state: High
 *  while it holds a lease. */
export function streamQualityOf(state: VideoProfileState, holder: string): StreamQuality {
  return state.leases?.some((l) => l.holder === holder) ? "high" : "auto";
}

/** The request that applies a quality for `holder`. */
export function streamQualityRequest(q: StreamQuality, holder: string): { path: string; method: "POST" | "DELETE"; body?: unknown } {
  return q === "high"
    ? { path: VIDEO_PROFILE_PATH, method: "POST", body: { profile: "hero", holder, lease_s: QUALITY_LEASE_S } }
    : { path: `${VIDEO_PROFILE_PATH}?holder=${encodeURIComponent(holder)}`, method: "DELETE" };
}

let renewTimer: number | undefined;

async function applyQuality(q: StreamQuality): Promise<VideoProfileState> {
  const r = streamQualityRequest(q, LEASE_OWNER);
  return apiFetch<VideoProfileState>(r.path, { method: r.method, body: r.body });
}

/** The stream quality field's state: null until known, and while the node has
 *  no profile route. */
export const useStreamQualityStore = create<{
  quality: StreamQuality | null;
  load: () => Promise<void>;
  set: (q: StreamQuality) => Promise<void>;
}>((set) => ({
  quality: null,
  load: async () => {
    if (isDemoMode()) {
      set({ quality: "auto" });
      return;
    }
    try {
      set({ quality: streamQualityOf(await apiFetch<VideoProfileState>(VIDEO_PROFILE_PATH), LEASE_OWNER) });
    } catch {
      set({ quality: null });
    }
  },
  set: async (q) => {
    window.clearInterval(renewTimer);
    renewTimer = undefined;
    if (isDemoMode()) {
      set({ quality: q });
      return;
    }
    try {
      set({ quality: streamQualityOf(await applyQuality(q), LEASE_OWNER) });
      if (q === "high") renewTimer = window.setInterval(() => void applyQuality("high").catch(() => undefined), QUALITY_RENEW_MS);
    } catch (e) {
      toast.err(e instanceof ApiError ? errorDetail(e.body, e.message) : String(e));
    }
  },
}));
