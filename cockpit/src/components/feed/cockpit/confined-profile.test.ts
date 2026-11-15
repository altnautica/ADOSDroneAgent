// @vitest-environment happy-dom
// The confined-place profile as the cockpit renders it, from the golden
// confined frame: the Confined badge beside the flight-state chip, the
// confined-near cap on the chip's line, the route-check report over the video
// while the route is too tight, the stall overlay hiding its own banners, and
// the Launch sheet's place picker sending `place_class:<class>` before the
// launch.

import { readFileSync } from "node:fs";
import path from "node:path";
import { createElement, type ReactElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const pilotLink = vi.hoisted(() => ({
  results: {} as Record<string, { ok: boolean; err?: string }>,
  calls: [] as string[],
}));
vi.mock("@/lib/pilot-link", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  sendPilotAction: (name: string) => {
    pilotLink.calls.push(name);
    return Promise.resolve(pilotLink.results[name] ?? { ok: true });
  },
  takePilotSession: () => true,
}));
vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  getConfig: () => Promise.resolve({ pilot: { obstacle_margin_m: 0.3, prop_tip_radius_m: 0.377 } }),
  getParam: () => Promise.resolve(null),
}));
// A hold fires on a click here; the hold itself has its own tests. The
// factory is hoisted above the imports, so it loads react itself.
vi.mock("@/components/ui/hold-button", async () => {
  const { createElement: h } = await import("react");
  return {
    HoldButton: ({ children, onFired, disabled }: { children: unknown; onFired: () => void; disabled?: boolean }) =>
      h("button", { type: "button", disabled, onClick: onFired, "data-testid": "hold" }, children as never),
  };
});
vi.mock("@/stores/voice-store", () => ({ speak: () => {} }));

import { FlightOverlays } from "@/components/feed/cockpit/flight-overlays";
import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { LaunchSheet } from "@/components/feed/cockpit/launch-sheet";
import { parsePilotState, type PilotState } from "@/shared/hud/cockpit-frames";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { usePreflightStore } from "@/stores/preflight-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const FIXTURES = path.resolve(__dirname, "../../../../../crates/ados-control/tests/fixtures");
const raw = (name: string) => JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8")) as Record<string, unknown>;
const confinedRaw = raw("pilot-frame-confined.json");
// The golden banner slot has long passed in wall-clock time: hold it open.
const confined = parsePilotState({ ...confinedRaw, banner: { ...(confinedRaw.banner as object), until_ms: null } })!;
const stallRaw = raw("pilot-frame-stall.json");
const stall = parsePilotState({ ...stallRaw, banner: { ...(stallRaw.banner as object), until_ms: null } })!;
const open = parsePilotState({ ...raw("pilot-frame.json"), armed: false, landed: true, sequence: null })!;

let host: HTMLDivElement;
let root: Root;

async function render(el: ReactElement): Promise<HTMLElement> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(el));
  return host;
}

const show = (p: PilotState) => useNavStreamStore.setState({ pilot: { data: p, at: Date.now() } });
// Sheets render into a portal on the body.
const q = (sel: string) => document.querySelector<HTMLElement>(sel);

beforeEach(() => {
  pilotLink.calls.length = 0;
  pilotLink.results = {};
  useCockpitUiStore.setState({ sheet: null, launchRefusal: null });
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  useNavStreamStore.setState({ pilot: null });
});

describe("the flight-state chip in a confined place", () => {
  it("shows the Confined badge and the confined-near cap", async () => {
    show(confined);
    await render(createElement(FlightStateChip, { compact: false, fit: false }));
    expect(q("[data-testid=confined-chip]")?.textContent).toBe("Confined");
    expect(q("[data-testid=flight-state-chip-lines]")?.textContent).toContain("Near the clearance limit: 0.5 m/s max");
  });

  it("shows no badge in an open place", async () => {
    show(open);
    await render(createElement(FlightStateChip, { compact: false, fit: false }));
    expect(q("[data-testid=confined-chip]")).toBeNull();
  });
});

describe("the flight overlays in a confined place", () => {
  it("report the too-tight route with its pinch, beside the route banner", async () => {
    show(confined);
    await render(createElement(FlightOverlays));
    const card = q("[data-testid=route-check-card]");
    expect(card?.getAttribute("data-state")).toBe("infeasible");
    expect(card?.getAttribute("role")).toBe("alert");
    expect(q("[data-testid=route-check-line]")?.textContent).toBe("Route too tight: leg 3→4 has 0.52 m, needs 0.58 m");
    expect(q("[data-testid=route-check-pinch]")?.textContent).toBe("At N 41.3 m, E -17.5 m, height 2.0 m");
    // The route banner says more than the stall overlay: it stays.
    expect(q("[data-testid=handover-banner]")?.textContent).toBe("Route too tight at leg 3→4: 0.52 m of 0.58 m · Holding");
    expect(q("[data-testid=mission-stall-banner]")).not.toBeNull();
  });

  it("show no report once the route is clear", async () => {
    show({ ...confined, routeCheck: { ...confined.routeCheck!, state: "feasible" } });
    await render(createElement(FlightOverlays));
    expect(q("[data-testid=route-check-card]")).toBeNull();
  });
});

describe("the stall overlay and its banners", () => {
  it("hides the stall banner while the overlay shows", async () => {
    show(stall);
    await render(createElement(FlightOverlays));
    expect(q("[data-testid=mission-stall-banner]")?.textContent).toContain("No progress to waypoint 3 for 15 s · Skipping in 33 s");
    expect(q("[data-testid=handover-banner]")).toBeNull();
  });

  it("hides a blocked-return banner behind a return stall", async () => {
    show({ ...stall, banner: { textKey: "pilot.banner.return_blocked", args: { in_s: 40 }, untilMs: null }, missionStall: { ...stall.missionStall!, kind: "return" } });
    await render(createElement(FlightOverlays));
    expect(q("[data-testid=handover-banner]")).toBeNull();
  });

  it("shows the banner again once the stall clears", async () => {
    show({ ...stall, missionStall: null });
    await render(createElement(FlightOverlays));
    expect(q("[data-testid=handover-banner]")).not.toBeNull();
  });
});

describe("the Launch sheet's place picker", () => {
  beforeEach(() => usePreflightStore.setState({ passed: true }));

  it("starts on the pilot's class and explains it with the effective margin", async () => {
    show(open);
    await render(createElement(LaunchSheet));
    await act(async () => {});
    expect(q("[data-place=open]")?.getAttribute("aria-checked")).toBe("true");
    expect(q("[data-testid=launch-place-explain]")?.textContent).toBe("Open sites: 0.5 m from obstacles.");
    await act(async () => q("[data-place=confined]")?.click());
    expect(q("[data-testid=launch-place-explain]")?.textContent).toBe(
      "Mines, tunnels and rooms: 0.48 m from walls, 0.5 m/s near the limit. Choose it only for a confined place.",
    );
  });

  it("sends place_class:confined before the launch", async () => {
    show(open);
    await render(createElement(LaunchSheet));
    await act(async () => q("[data-place=confined]")?.click());
    await act(async () => q("[data-testid=hold]")?.click());
    expect(pilotLink.calls).toEqual(["place_class:confined", "launch"]);
  });

  it("sends no class change when the class stands", async () => {
    show(open);
    await render(createElement(LaunchSheet));
    await act(async () => q("[data-testid=hold]")?.click());
    expect(pilotLink.calls).toEqual(["launch"]);
  });

  it("stops at a refused class change and says why", async () => {
    pilotLink.results["place_class:confined"] = { ok: false, err: "armed" };
    show(open);
    await render(createElement(LaunchSheet));
    await act(async () => q("[data-place=confined]")?.click());
    await act(async () => q("[data-testid=hold]")?.click());
    expect(pilotLink.calls).toEqual(["place_class:confined"]);
    expect(useCockpitUiStore.getState().launchRefusal).toBe(specText("flow.launch.refused", { reason: refusalText("armed") }));
  });

  it("shows a refused confined launch through the refusal table", async () => {
    pilotLink.results.launch = { ok: false, err: "route_infeasible" };
    show({ ...confined, armed: false, landed: true, sequence: null, missionStall: null });
    await render(createElement(LaunchSheet));
    expect(q("[data-testid=route-check-card]")?.getAttribute("data-state")).toBe("infeasible");
    await act(async () => q("[data-testid=hold]")?.click());
    expect(pilotLink.calls).toEqual(["launch"]);
    expect(useCockpitUiStore.getState().launchRefusal).toBe(specText("flow.launch.refused", { reason: "Route too tight for the confined profile" }));
  });
});
