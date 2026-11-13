import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LEASE_OWNER } from "@/lib/lease-owner";
import { PANEL_HERO_HOLD, panelWantsHero } from "@/lib/video-hero-hold";
import { apiFetch } from "@/shared/api-fetch";
import { bindHeroHold, HERO_RELEASE_GRACE_MS, VIDEO_PROFILE_PATH, type VisibilitySource } from "@/shared/hero-hold";

vi.mock("@/shared/api-fetch", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  apiFetch: vi.fn(),
}));

const fetchMock = vi.mocked(apiFetch);
const calls = () => fetchMock.mock.calls.map(([p, opts]) => `${opts?.method ?? "GET"} ${p}`);

beforeEach(() => {
  vi.useFakeTimers();
  fetchMock.mockReset();
  fetchMock.mockResolvedValue({ profile: "hero", leases: [{ holder: LEASE_OWNER }] });
});
afterEach(() => {
  vi.useRealTimers();
});

describe("the Video panel's hero hold", () => {
  it("is taken while the page is visible and released when it is hidden", async () => {
    const listeners = new Set<() => void>();
    const page = {
      visibilityState: "visible",
      addEventListener: (_t: string, cb: () => void) => void listeners.add(cb),
      removeEventListener: (_t: string, cb: () => void) => void listeners.delete(cb),
    };
    const unbind = bindHeroHold(PANEL_HERO_HOLD, page as VisibilitySource);
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledWith(VIDEO_PROFILE_PATH, {
      method: "POST",
      body: { profile: "hero", holder: LEASE_OWNER, lease_s: 30 },
      skipAuthSignal: true,
    });

    page.visibilityState = "hidden";
    for (const cb of [...listeners]) cb();
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    const release = `DELETE ${VIDEO_PROFILE_PATH}?holder=${encodeURIComponent(LEASE_OWNER)}`;
    expect(calls()).toContain(release);

    page.visibilityState = "visible";
    for (const cb of [...listeners]) cb();
    await vi.advanceTimersByTimeAsync(0);
    expect(calls().filter((c) => c.startsWith("POST"))).toHaveLength(2);

    // Unmounting the panel lets go too.
    unbind();
    await vi.advanceTimersByTimeAsync(HERO_RELEASE_GRACE_MS);
    expect(calls().filter((c) => c === release)).toHaveLength(2);
  });

  it("is wanted while the panel has a stream to show on a node that serves the profile", () => {
    expect(panelWantsHero(true, "drone")).toBe(true);
    expect(panelWantsHero(false, "drone")).toBe(false);
    expect(panelWantsHero(true, "ground_station")).toBe(false);
    expect(panelWantsHero(true, null)).toBe(false);
  });

  it("is held by the Video panel itself", () => {
    const panel = readFileSync(path.resolve(__dirname, "../components/panels/video-panel.tsx"), "utf8");
    expect(panel).toMatch(/useHeroHold\(PANEL_HERO_HOLD, panelWantsHero\(/);
  });
});
