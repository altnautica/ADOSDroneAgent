import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError, apiFetch } from "./api-fetch";
import {
  bindHeroHold,
  createHeroHold,
  HERO_LEASE_S,
  HERO_RELEASE_GRACE_MS,
  HERO_RENEW_MS,
  VIDEO_PROFILE_PATH,
  type VisibilitySource,
} from "./hero-hold";

vi.mock("./api-fetch", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  apiFetch: vi.fn(),
}));

const fetchMock = vi.mocked(apiFetch);
const flush = () => vi.advanceTimersByTimeAsync(0);

/** The calls made so far, as `METHOD path`. */
const calls = () => fetchMock.mock.calls.map(([path, opts]) => `${opts?.method ?? "GET"} ${path}`);
const posts = () => calls().filter((c) => c.startsWith("POST"));

function fakeDocument(state: "visible" | "hidden") {
  const listeners = new Set<() => void>();
  const doc = {
    visibilityState: state as string,
    addEventListener: (_t: string, cb: () => void) => void listeners.add(cb),
    removeEventListener: (_t: string, cb: () => void) => void listeners.delete(cb),
  };
  return {
    doc: doc as VisibilitySource,
    listeners,
    set(next: "visible" | "hidden") {
      doc.visibilityState = next;
      for (const cb of [...listeners]) cb();
    },
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  fetchMock.mockReset();
  fetchMock.mockResolvedValue({ profile: "hero", leases: [] });
});
afterEach(() => {
  vi.useRealTimers();
});

describe("createHeroHold", () => {
  it("takes the lease at once and renews it at a third of the lease", async () => {
    const hold = createHeroHold("t:renew");
    hold.acquire();
    await flush();
    expect(fetchMock).toHaveBeenCalledWith(VIDEO_PROFILE_PATH, {
      method: "POST",
      body: { profile: "hero", holder: "t:renew", lease_s: HERO_LEASE_S },
      skipAuthSignal: true,
    });
    expect(HERO_RENEW_MS).toBe((HERO_LEASE_S * 1000) / 3);
    await vi.advanceTimersByTimeAsync(HERO_RENEW_MS);
    expect(posts()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(HERO_RENEW_MS * 2);
    expect(posts()).toHaveLength(4);
  });

  it("retries a failed renewal within 2 s, backing off, instead of dropping it", async () => {
    const hold = createHeroHold("t:retry");
    hold.acquire();
    await flush();
    fetchMock.mockRejectedValueOnce(new ApiError("503", 503, null));
    fetchMock.mockRejectedValueOnce(new TypeError("network"));
    await vi.advanceTimersByTimeAsync(HERO_RENEW_MS);
    expect(posts()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1000);
    expect(posts()).toHaveLength(3);
    // The second failure waits longer before the next try.
    await vi.advanceTimersByTimeAsync(1999);
    expect(posts()).toHaveLength(3);
    await vi.advanceTimersByTimeAsync(1);
    expect(posts()).toHaveLength(4);
    // That one succeeded: back to the renewal cadence.
    await vi.advanceTimersByTimeAsync(HERO_RENEW_MS - 1);
    expect(posts()).toHaveLength(4);
    await vi.advanceTimersByTimeAsync(1);
    expect(posts()).toHaveLength(5);
  });

  it("backs off and retries a rate-limited promotion", async () => {
    fetchMock.mockRejectedValueOnce(new ApiError("429", 429, null));
    const hold = createHeroHold("t:429");
    hold.acquire();
    await flush();
    expect(posts()).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(posts()).toHaveLength(2);
  });

  it("stops quietly on a node without the route", async () => {
    fetchMock.mockRejectedValueOnce(new ApiError("404", 404, null));
    const hold = createHeroHold("t:404");
    const release = hold.acquire();
    await vi.advanceTimersByTimeAsync(120_000);
    expect(calls()).toEqual([`POST ${VIDEO_PROFILE_PATH}`]);
    release();
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    expect(calls()).toEqual([`POST ${VIDEO_PROFILE_PATH}`]);
  });

  it("is shared by its holders and released a short grace after the last lets go", async () => {
    const hold = createHeroHold("t:grace");
    const a = hold.acquire();
    const b = hold.acquire();
    await flush();
    expect(posts()).toHaveLength(1);
    a();
    a();
    b();
    // A new holder inside the grace keeps the lease without a new request.
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS - 1);
    const c = hold.acquire();
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    expect(calls().filter((x) => x.startsWith("DELETE"))).toEqual([]);
    expect(posts()).toHaveLength(1);
    c();
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    expect(calls()).toContain(`DELETE ${VIDEO_PROFILE_PATH}?holder=t%3Agrace`);
    const after = calls().length;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls()).toHaveLength(after);
    expect(hold.active()).toBe(false);
  });

  it("renews at once on resume", async () => {
    const hold = createHeroHold("t:resume");
    hold.acquire();
    await flush();
    await vi.advanceTimersByTimeAsync(4000);
    hold.resume();
    await flush();
    expect(posts()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(HERO_RENEW_MS);
    expect(posts()).toHaveLength(3);
  });
});

describe("bindHeroHold", () => {
  it("holds only while the page is visible", async () => {
    const hold = createHeroHold("t:vis");
    const page = fakeDocument("hidden");
    const unbind = bindHeroHold(hold, page.doc);
    await flush();
    expect(calls()).toEqual([]);
    page.set("visible");
    await flush();
    expect(posts()).toHaveLength(1);
    page.set("hidden");
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    expect(calls()).toContain(`DELETE ${VIDEO_PROFILE_PATH}?holder=t%3Avis`);
    page.set("visible");
    await flush();
    expect(posts()).toHaveLength(2);
    unbind();
    expect(page.listeners.size).toBe(0);
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    expect(calls().filter((x) => x.startsWith("DELETE"))).toHaveLength(2);
  });
});
