// The one `GET /api/v1/nav/status` poller every dashboard reader shares (the
// flight chip, the status card, the video layers). One query key, so a page
// with the chip and the video panel makes one request per tick:
//   - 5 Hz while a video panel is mounted, else 1 Hz;
//   - while the panel shows live video its layers lease a view
//     (`?view=main:<layers>`), so the agent computes them for that camera.
// Each request renews the lease; leaving the panel drops it.

import { useQuery } from "@tanstack/react-query";
import { useEffect } from "react";
import { create } from "zustand";

import { apiFetch } from "@/shared/api-fetch";

export const NAV_POLL_IDLE_MS = 1_000;
export const NAV_POLL_VIDEO_MS = 200;
export const NAV_STATUS_KEY = ["nav-status"] as const;

export interface NavStatusEntry {
  ts_ms?: number;
  /** Age at the agent when served; absent on an agent that does not report it. */
  age_ms?: number;
  data: unknown;
}

export type NavStatusBody = Partial<
  Record<"pilot" | "ar" | "obstacles" | "depth" | "surfaces" | "follow", NavStatusEntry | null>
>;

interface NavPollState {
  /** Mounted video panels. */
  videoPanels: number;
  /** The leased view, or null. */
  view: string | null;
  addPanel: (d: 1 | -1) => void;
  setView: (v: string | null) => void;
}

export const useNavPollStore = create<NavPollState>((set) => ({
  videoPanels: 0,
  view: null,
  addPanel: (d) => set((s) => ({ videoPanels: Math.max(0, s.videoPanels + d) })),
  setView: (view) => set({ view }),
}));

/** The request path for a leased view, or the plain status read. */
export function navStatusPath(view: string | null): string {
  return view ? `/api/v1/nav/status?view=${encodeURIComponent(view)}` : "/api/v1/nav/status";
}

/** The poll interval: 5 Hz with a video panel mounted, else 1 Hz. */
export function navPollMs(videoPanels: number): number {
  return videoPanels > 0 ? NAV_POLL_VIDEO_MS : NAV_POLL_IDLE_MS;
}

export function useNavStatus() {
  const panels = useNavPollStore((s) => s.videoPanels);
  return useQuery<NavStatusBody>({
    queryKey: NAV_STATUS_KEY,
    queryFn: ({ signal }) => apiFetch<NavStatusBody>(navStatusPath(useNavPollStore.getState().view), { signal }),
    refetchInterval: navPollMs(panels),
    retry: false,
  });
}

/** Marks a mounted video panel (the poll runs at 5 Hz while one is). */
export function useNavVideoPanel(): void {
  useEffect(() => {
    useNavPollStore.getState().addPanel(1);
    return () => useNavPollStore.getState().addPanel(-1);
  }, []);
}

/** Leases `view` through the shared poll while non-null. */
export function useNavLease(view: string | null): void {
  useEffect(() => {
    if (!view) return;
    useNavPollStore.getState().setView(view);
    return () => {
      if (useNavPollStore.getState().view === view) useNavPollStore.getState().setView(null);
    };
  }, [view]);
}
