import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";

import { LEASE_OWNER } from "@/lib/lease-owner";
import { defaultLeasedLayers, holdLayerDemand, layersViewQuery, LEASED_LAYERS, type VisionDemandBody } from "@/lib/video-layer-lease";
import {
  NAV_POLL_IDLE_MS,
  NAV_POLL_VIDEO_MS,
  NAV_STATUS_TIMEOUT_MS,
  NavPoller,
  navPollMs,
  navStatusPath,
  navStatusReaderOptions,
  useNavPollStore,
  type NavStatusBody,
} from "@/hooks/use-nav-status";

describe("one nav-status poller for every reader", () => {
  afterEach(() => {
    vi.useRealTimers();
    useNavPollStore.setState({ videoPanels: 0, view: null });
  });

  /** A reader as `useNavStatus` mounts one: a cache observer plus a hold on the poller. */
  function mountReader(client: QueryClient, poller: NavPoller) {
    const observer = new QueryObserver<NavStatusBody>(client, navStatusReaderOptions);
    const unsubscribe = observer.subscribe(() => undefined);
    const release = poller.acquire();
    return {
      observer,
      unmount: () => {
        unsubscribe();
        release();
      },
    };
  }

  it("never lets a reader fetch or poll on its own", () => {
    expect(navStatusReaderOptions).toMatchObject({ enabled: false, refetchInterval: false, retry: false });
    // A mounted reader with no poller makes no request at all.
    vi.useFakeTimers();
    const client = new QueryClient();
    const queryFn = vi.fn(() => Promise.resolve({}));
    const observer = new QueryObserver<NavStatusBody>(client, { ...navStatusReaderOptions, queryFn });
    const unsubscribe = observer.subscribe(() => undefined);
    vi.advanceTimersByTime(5_000);
    unsubscribe();
    expect(queryFn).not.toHaveBeenCalled();
  });

  it("makes one request per tick with two readers mounted out of phase", async () => {
    vi.useFakeTimers();
    const client = new QueryClient();
    const paths: string[] = [];
    const poller = new NavPoller(client, (path) => {
      paths.push(path);
      return Promise.resolve({ pilot: { age_ms: 0, data: { n: paths.length } } });
    });
    useNavPollStore.setState({ videoPanels: 1, view: "main:ar_route" });
    const chip = mountReader(client, poller);
    await vi.advanceTimersByTimeAsync(100);
    const card = mountReader(client, poller);
    expect(paths.length).toBe(1);
    await vi.advanceTimersByTimeAsync(1_000);
    // 5 Hz: five ticks in a second, one request each.
    expect(paths.length).toBe(6);
    expect(paths.every((p) => p === "/api/v1/nav/status?view=main%3Aar_route")).toBe(true);
    expect(chip.observer.getCurrentResult().data).toEqual(card.observer.getCurrentResult().data);
    expect(card.observer.getCurrentResult().data?.pilot?.data).toEqual({ n: 6 });

    // No video panel: 1 Hz.
    useNavPollStore.setState({ videoPanels: 0 });
    await vi.advanceTimersByTimeAsync(2_000);
    expect(paths.length).toBe(8);

    // The poll stops with the last reader, not the first.
    chip.unmount();
    await vi.advanceTimersByTimeAsync(1_000);
    expect(paths.length).toBe(9);
    card.unmount();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(paths.length).toBe(9);
    client.clear();
  });
});

describe("shared nav-status poll", () => {
  // Real timers: AbortSignal.timeout runs on the runtime's own clock.
  it("abandons a hung read after the timeout so the poll keeps going", async () => {
    const client = new QueryClient();
    let calls = 0;
    const poller = new NavPoller(client, (_path, signal) => {
      calls += 1;
      // Never answers; only the abort ends it.
      return new Promise((_, reject) => signal.addEventListener("abort", () => reject(signal.reason)));
    });
    useNavPollStore.setState({ videoPanels: 1, view: null });
    const release = poller.acquire();
    expect(calls).toBe(1);
    await new Promise((r) => setTimeout(r, NAV_STATUS_TIMEOUT_MS + 300));
    expect(calls).toBeGreaterThan(1);
    release();
  });

  it("polls at 5 Hz while a video panel is mounted, else 1 Hz", () => {
    expect(navPollMs(1)).toBe(NAV_POLL_VIDEO_MS);
    expect(NAV_POLL_VIDEO_MS).toBe(200);
    expect(navPollMs(0)).toBe(NAV_POLL_IDLE_MS);
    expect(NAV_POLL_IDLE_MS).toBe(1000);
  });

  it("carries the leased view on the one status path", () => {
    expect(navStatusPath(null)).toBe("/api/v1/nav/status");
    expect(navStatusPath("main:ar_route,depth")).toBe("/api/v1/nav/status?view=main%3Aar_route%2Cdepth");
  });
});

describe("dashboard video layer toggles", () => {
  it("offers depth and surfaces, off by default as layers.json defaults them", () => {
    expect(LEASED_LAYERS.map((l) => l.id)).toEqual(["depth", "surfaces"]);
    expect(defaultLeasedLayers()).toEqual({ depth: false, surfaces: false });
    expect(layersViewQuery(defaultLeasedLayers())).toBe("main:ar_route,ar_motion,obstacles");
  });

  it("leases a toggled-on layer through the same view", () => {
    expect(layersViewQuery({ depth: true, surfaces: false })).toBe("main:ar_route,ar_motion,obstacles,depth");
    expect(layersViewQuery({ depth: true, surfaces: true })).toBe("main:ar_route,ar_motion,obstacles,depth,surfaces");
  });

  it("holds a vision demand of the toggled kind, renewed every 2 s and released with hold 0", () => {
    vi.useFakeTimers();
    const sent: VisionDemandBody[] = [];
    const doc = { visibilityState: "visible" as DocumentVisibilityState, addEventListener: () => undefined, removeEventListener: () => undefined };
    const release = holdLayerDemand("depth", (b) => sent.push(b), doc);
    expect(sent).toEqual([{ kind: "depth", camera_id: "main", hold_s: 6, owner: LEASE_OWNER }]);
    vi.advanceTimersByTime(4_000);
    expect(sent.map((b) => b.hold_s)).toEqual([6, 6, 6]);
    release();
    expect(sent.at(-1)).toEqual({ kind: "depth", camera_id: "main", hold_s: 0, owner: LEASE_OWNER });
    vi.advanceTimersByTime(10_000);
    expect(sent.length).toBe(4);
    vi.useRealTimers();
  });
});
