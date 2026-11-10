// The one `GET /api/v1/nav/status` poller every dashboard reader shares (the
// flight chip, the status card, the video layers). One owner holds the only
// interval (`NavPoller`, one per query client) while any reader is mounted;
// readers subscribe to its cache entry and never fetch on their own, so a page
// with the chip and the video panel makes one request per tick:
//   - 5 Hz while a video panel is mounted, else 1 Hz;
//   - while the panel shows live video its layers lease a view
//     (`?view=main:<layers>`), so the agent computes them for that camera.
// Each request renews the lease; leaving the panel drops it. A hidden page
// skips its ticks.

import { focusManager, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
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

export type NavStatusFetch = (path: string, signal: AbortSignal) => Promise<NavStatusBody>;

/** A nav-status read that has not answered by now is abandoned, so one hung
 *  request cannot hold the joined fetch (and so the poll) forever. */
export const NAV_STATUS_TIMEOUT_MS = 1000;

const fetchNavStatus: NavStatusFetch = (path, signal) => apiFetch<NavStatusBody>(path, { signal });

/** The query's own signal, also aborted after `NAV_STATUS_TIMEOUT_MS`. */
function withTimeout(signal: AbortSignal): AbortSignal {
  return AbortSignal.any([signal, AbortSignal.timeout(NAV_STATUS_TIMEOUT_MS)]);
}

/** What every reader subscribes with: the shared cache entry, never a fetch
 *  of its own (the poller is the only fetcher). */
export const navStatusReaderOptions = {
  queryKey: NAV_STATUS_KEY,
  queryFn: ({ signal }: { signal: AbortSignal }) => fetchNavStatus(navStatusPath(useNavPollStore.getState().view), withTimeout(signal)),
  enabled: false,
  refetchInterval: false,
  retry: false,
} as const;

/** The single owner of the nav-status interval for one query client: runs
 *  while at least one reader holds it, at the rate `navPollMs` gives for the
 *  mounted video panels. */
export class NavPoller {
  private readers = 0;
  private timer: ReturnType<typeof setInterval> | null = null;
  private ms = 0;
  private unsubscribe: (() => void) | null = null;

  constructor(
    private readonly client: QueryClient,
    private readonly fetchBody: NavStatusFetch = fetchNavStatus,
  ) {}

  /** Hold the poll; the returned function releases this hold once. */
  acquire(): () => void {
    this.readers += 1;
    if (this.readers === 1) this.start();
    let held = true;
    return () => {
      if (!held) return;
      held = false;
      this.readers -= 1;
      if (this.readers === 0) this.stop();
    };
  }

  private start(): void {
    this.unsubscribe = useNavPollStore.subscribe(() => this.schedule());
    this.tick();
    this.schedule();
  }

  private stop(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
    clearInterval(this.timer ?? undefined);
    this.timer = null;
    this.ms = 0;
  }

  private schedule(): void {
    const ms = navPollMs(useNavPollStore.getState().videoPanels);
    if (ms === this.ms && this.timer !== null) return;
    clearInterval(this.timer ?? undefined);
    this.ms = ms;
    this.timer = setInterval(() => this.tick(), ms);
  }

  private tick(): void {
    if (!focusManager.isFocused()) return;
    // A fetch still in flight is joined, not repeated.
    void this.client
      .fetchQuery({
        queryKey: NAV_STATUS_KEY,
        queryFn: ({ signal }) => this.fetchBody(navStatusPath(useNavPollStore.getState().view), withTimeout(signal)),
        staleTime: 0,
        retry: false,
      })
      .catch(() => undefined);
  }
}

const pollers = new WeakMap<QueryClient, NavPoller>();

/** The poller of `client`, made on first use. */
export function navPoller(client: QueryClient): NavPoller {
  let p = pollers.get(client);
  if (!p) {
    p = new NavPoller(client);
    pollers.set(client, p);
  }
  return p;
}

/** The shared nav-status read; holds the one poller while mounted. */
export function useNavStatus() {
  const client = useQueryClient();
  useEffect(() => navPoller(client).acquire(), [client]);
  return useQuery<NavStatusBody>(navStatusReaderOptions);
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
