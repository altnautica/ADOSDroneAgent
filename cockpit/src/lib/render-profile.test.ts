import { describe, expect, it } from "vitest";

import {
  pollIntervalMs,
  renderProfileClass,
  renderProfileFrom,
} from "@/lib/render-profile";

describe("renderProfileFrom", () => {
  it("reads the minimal profile the kiosk has been appending all along", () => {
    // The kiosk has appended this for a long time and nothing read it, so the
    // flag — and its automatic low-RAM trigger — did nothing on screen.
    expect(renderProfileFrom("?layer=minimal")).toBe("minimal");
  });

  it("defaults to full with no parameter", () => {
    expect(renderProfileFrom("")).toBe("full");
    expect(renderProfileFrom("?demo=1")).toBe("full");
  });

  it("degrades a typo to the richer mode rather than stripping the UI", () => {
    expect(renderProfileFrom("?layer=miniml")).toBe("full");
    expect(renderProfileFrom("?layer=")).toBe("full");
  });

  it("survives other parameters alongside it", () => {
    // The kiosk and a reach link can both put parameters on this URL.
    expect(renderProfileFrom("?key=abc123&layer=minimal&demo=1")).toBe("minimal");
  });
});

describe("renderProfileClass", () => {
  it("marks the shell root only in minimal mode", () => {
    expect(renderProfileClass("minimal")).toBe("layer-minimal");
    expect(renderProfileClass("full")).toBe("");
  });
});

describe("pollIntervalMs", () => {
  it("slows polling in minimal mode", () => {
    expect(pollIntervalMs(200, "minimal")).toBeGreaterThan(200);
  });

  it("leaves full mode exactly as it was", () => {
    // A no-op in the default profile, so this cannot change shipped behaviour
    // for anyone not asking for minimal.
    expect(pollIntervalMs(200, "full")).toBe(200);
    expect(pollIntervalMs(400, "full")).toBe(400);
  });

  it("never returns a zero or negative interval", () => {
    for (const base of [1, 50, 200, 400, 1000]) {
      for (const profile of ["full", "minimal"] as const) {
        expect(pollIntervalMs(base, profile)).toBeGreaterThan(0);
      }
    }
  });
});
