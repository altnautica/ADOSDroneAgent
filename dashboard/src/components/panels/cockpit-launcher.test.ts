import { readFileSync } from "node:fs";
import path from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import { enterCockpit } from "@/lib/cockpit-launch";

const SRC = readFileSync(path.resolve(__dirname, "cockpit-launcher.tsx"), "utf8");
const BADGE = readFileSync(path.resolve(__dirname, "../ui/badge.tsx"), "utf8");

describe("Enter Cockpit row", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("navigates to /cockpit without asking for fullscreen", () => {
    const assign = vi.fn();
    const requestFullscreen = vi.fn(() => Promise.resolve());
    vi.stubGlobal("window", { location: { assign } });
    vi.stubGlobal("document", { documentElement: { requestFullscreen }, fullscreenElement: null });
    enterCockpit();
    expect(assign).toHaveBeenCalledWith("/cockpit");
    expect(requestFullscreen).not.toHaveBeenCalled();
  });

  it("is a plain card row with a neutral tile, no amber gradient", () => {
    expect(SRC).toMatch(/rounded-md border border-border bg-card/);
    expect(SRC).not.toMatch(/amber|gradient/);
    expect(SRC).not.toMatch(/fullscreen"/);
  });
});

describe("Badge", () => {
  it("is an outline badge without forced uppercase or tracking", () => {
    expect(BADGE).toMatch(/rounded-\[4px\] border bg-transparent/);
    expect(BADGE).not.toMatch(/uppercase"|tracking-/);
    for (const v of ["success", "warning", "serious", "error", "info", "neutral"]) {
      expect(BADGE).toMatch(new RegExp(`${v}: "`));
    }
    expect(BADGE).toMatch(/sm: "h-5 px-1\.5 text-\[11px\]"/);
    expect(BADGE).toMatch(/md: "h-6 px-2 text-xs"/);
  });
});
