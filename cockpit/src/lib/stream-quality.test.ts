// @vitest-environment happy-dom
// The Feed's hero hold: the Feed takes this window's hero lease on every
// stream quality (Auto included) while it shows, renews it, retries a failed
// renewal, and lets go when it unmounts; High pins a second lease that load()
// picks back up when the node lists it.

import { readFileSync } from "node:fs";
import path from "node:path";
import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LEASE_OWNER } from "@/lib/lease-owner";
import { FEED_HOLDER, streamQualityOf, useFeedHeroHold, useStreamQualityStore } from "@/lib/stream-quality";
import { ApiError, apiFetch } from "@/shared/api-fetch";
import { HERO_LEASE_S, HERO_RELEASE_GRACE_MS, HERO_RENEW_MS, VIDEO_PROFILE_PATH } from "@/shared/hero-hold";

vi.mock("@/shared/api-fetch", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  apiFetch: vi.fn(),
}));

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const fetchMock = vi.mocked(apiFetch);
const flush = () => act(() => vi.advanceTimersByTimeAsync(0));
const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

/** The requests so far for `holder`: `POST` (create/renew) or `DELETE`. */
function requests(holder: string): string[] {
  return fetchMock.mock.calls.flatMap(([p, opts]) => {
    const body = opts?.body;
    const bodyHolder = body && typeof body === "object" && "holder" in body ? body.holder : undefined;
    if (opts?.method === "POST" && bodyHolder === holder) return ["POST"];
    if (opts?.method === "DELETE" && p === `${VIDEO_PROFILE_PATH}?holder=${encodeURIComponent(holder)}`) return ["DELETE"];
    return [];
  });
}

function FeedSurface() {
  useFeedHeroHold();
  return null;
}

let host: HTMLDivElement | null = null;
let root: Root;

async function mountFeed() {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(FeedSurface)));
}

async function unmountFeed() {
  if (!host) return;
  await act(async () => root.unmount());
  host.remove();
  host = null;
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "setTimeout", "clearTimeout", "Date"] });
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "visible" });
  window.history.replaceState(null, "", "/cockpit/");
  fetchMock.mockReset();
  fetchMock.mockResolvedValue({ profile: "hero", leases: [] });
  useStreamQualityStore.setState({ quality: null });
});

afterEach(async () => {
  await unmountFeed();
  window.history.replaceState(null, "", "/cockpit/");
  await act(() => useStreamQualityStore.getState().set("auto"));
  await advance(HERO_RELEASE_GRACE_MS * 2);
  vi.useRealTimers();
});

describe("the Feed's hero hold", () => {
  it("is taken on Auto when the Feed mounts, renewed, and released after it unmounts", async () => {
    expect(useStreamQualityStore.getState().quality).not.toBe("high");
    await mountFeed();
    await flush();
    expect(fetchMock).toHaveBeenCalledWith(VIDEO_PROFILE_PATH, {
      method: "POST",
      body: { profile: "hero", holder: FEED_HOLDER, lease_s: HERO_LEASE_S },
      skipAuthSignal: true,
    });
    await advance(HERO_RENEW_MS);
    expect(requests(FEED_HOLDER)).toEqual(["POST", "POST"]);
    await unmountFeed();
    await advance(HERO_RELEASE_GRACE_MS);
    expect(requests(FEED_HOLDER)).toEqual(["POST", "POST", "DELETE"]);
  });

  it("retries a failed renewal within 2 s rather than letting the lease lapse", async () => {
    await mountFeed();
    await flush();
    fetchMock.mockRejectedValueOnce(new ApiError("503", 503, null));
    await advance(HERO_RENEW_MS);
    expect(requests(FEED_HOLDER)).toEqual(["POST", "POST"]);
    await advance(2000);
    expect(requests(FEED_HOLDER)).toEqual(["POST", "POST", "POST"]);
  });

  it("names a holder distinct from this window's pinned lease", () => {
    expect(FEED_HOLDER).not.toBe(LEASE_OWNER);
    expect(FEED_HOLDER.startsWith(LEASE_OWNER)).toBe(true);
    expect(FEED_HOLDER).toMatch(/^[A-Za-z0-9._:-]{1,64}$/);
  });

  it("is never taken in demo mode", async () => {
    window.history.replaceState(null, "", "/cockpit/?demo=1");
    await mountFeed();
    await advance(HERO_RENEW_MS * 2);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("lives on the Feed screen itself, which the kiosk display runs too", () => {
    const feed = readFileSync(path.resolve(__dirname, "../components/screens/feed-screen.tsx"), "utf8");
    expect(feed).toContain("useFeedHeroHold()");
  });
});

describe("the High stream quality (a pinned lease)", () => {
  it("pins this window's lease and Auto releases it", async () => {
    fetchMock.mockResolvedValue({ profile: "hero", leases: [{ holder: LEASE_OWNER }] });
    await act(() => useStreamQualityStore.getState().set("high"));
    expect(useStreamQualityStore.getState().quality).toBe("high");
    expect(requests(LEASE_OWNER)).toEqual(["POST"]);
    await advance(HERO_RENEW_MS);
    expect(requests(LEASE_OWNER)).toEqual(["POST", "POST"]);
    fetchMock.mockResolvedValue({ profile: "thumbnail", leases: [] });
    await act(() => useStreamQualityStore.getState().set("auto"));
    await flush();
    expect(useStreamQualityStore.getState().quality).toBe("auto");
    expect(requests(LEASE_OWNER)).toEqual(["POST", "POST", "DELETE"]);
  });

  it("load() restarts the renewal when the node already lists this window's lease", async () => {
    fetchMock.mockImplementation(async (_p, opts) =>
      opts?.method === "DELETE" ? { leases: [] } : { profile: "hero", leases: [{ holder: LEASE_OWNER }] },
    );
    await act(() => useStreamQualityStore.getState().load());
    expect(useStreamQualityStore.getState().quality).toBe("high");
    await flush();
    expect(requests(LEASE_OWNER)).toEqual(["POST"]);
    await advance(HERO_RENEW_MS * 2);
    expect(requests(LEASE_OWNER)).toEqual(["POST", "POST", "POST"]);
  });

  it("reads High only from this window's pinned lease", () => {
    expect(streamQualityOf({ leases: [{ holder: LEASE_OWNER }] }, LEASE_OWNER)).toBe("high");
    expect(streamQualityOf({ leases: [{ holder: FEED_HOLDER }] }, LEASE_OWNER)).toBe("auto");
  });
});
