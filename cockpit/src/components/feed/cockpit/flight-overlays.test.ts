// @vitest-environment happy-dom
// The flight overlays as the real component renders them: tips hold while a
// launch step, handover, link-loss, low-battery, mission stall or sticks
// banner is up and never sit over one, a refused Abort shows its reason on
// screen, and the golden stall frame renders its overlay and sends each
// action.

import { readFileSync } from "node:fs";
import path from "node:path";
import { createElement, Fragment } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const pilotLink = vi.hoisted(() => ({
  result: { ok: false, err: "busy" } as { ok: boolean; err?: string },
  calls: [] as string[],
}));
vi.mock("@/lib/pilot-link", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  sendPilotAction: (name: string) => {
    pilotLink.calls.push(name);
    return Promise.resolve(pilotLink.result);
  },
  takePilotSession: () => true,
}));
vi.mock("@/stores/voice-store", () => ({ speak: () => {} }));

import { FlightOverlays } from "@/components/feed/cockpit/flight-overlays";
import { SkillAckLine } from "@/components/feed/skill-picker/skill-picker";
import { tipAvoidRects } from "@/components/feed/tip-rects";
import { parsePilotState, type PilotState } from "@/shared/hud/cockpit-frames";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useConfirmStore } from "@/stores/confirm-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { alertBannerVisible } from "@/stores/tips-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const IDLE = {
  armed: true,
  landed: true,
  sequence: null,
  rthCountdownS: null,
  linkLossInS: null,
  banner: null,
  gate: null,
  inputSession: null,
} as unknown as PilotState;

function pilot(over: Record<string, unknown>): void {
  useNavStreamStore.setState({ pilot: { data: { ...IDLE, ...over } as PilotState, at: Date.now() } });
}

let host: HTMLDivElement;
let root: Root;

async function mount(): Promise<void> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(Fragment, null, createElement(FlightOverlays), createElement(SkillAckLine))));
}

beforeEach(() => {
  useConfirmStore.getState().setAck(null);
  useCockpitUiStore.setState({ sheet: null, launchRefusal: null });
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  useNavStreamStore.setState({ pilot: null });
});

const SEQ = { step: "climb", error: null };

describe("tips and the flight overlays", () => {
  const cases: [string, Record<string, unknown>][] = [
    ["a launch step with its Abort", { sequence: SEQ }],
    ["the low-battery countdown", { rthCountdownS: 9 }],
    ["the link-loss countdown", { linkLossInS: 5 }],
    ["sticks with another surface", { inputSession: { session: "other", src: "gcs", label: "Ground station" } }],
  ];
  for (const [name, over] of cases) {
    it(`hold while ${name} is up`, async () => {
      pilot(over);
      await mount();
      expect(document.querySelector("[data-testid=flight-overlays]")?.children.length).toBeGreaterThan(0);
      expect(alertBannerVisible(document, () => true)).toBe(true);
    });
  }

  it("count nothing as a banner while the overlays are empty", async () => {
    pilot({});
    await mount();
    expect(alertBannerVisible(document, () => true)).toBe(false);
  });

  it("keep a tip off the overlays' area", async () => {
    pilot({ sequence: SEQ });
    await mount();
    const mark = { x: 500, y: 40, w: 300, h: 60 };
    const rects = tipAvoidRects((el) => (el.getAttribute("data-testid") === "flight-overlays" ? mark : null));
    expect(rects).toContainEqual(mark);
  });
});

describe("a refused Abort from the flight overlay", () => {
  it("shows its reason on screen with the Launch sheet closed", async () => {
    pilotLink.result = { ok: false, err: "busy" };
    pilot({ sequence: SEQ });
    await mount();
    const abort = document.querySelector<HTMLButtonElement>("[data-testid=launch-abort]");
    await act(async () => abort?.click());
    expect(pilotLink.calls).toContain("pause");
    const text = specText("flow.launch.abort_refused", { reason: refusalText("busy") });
    expect(useConfirmStore.getState().ack).toEqual({ text, kind: "err" });
    expect(document.body.textContent).toContain(text);
  });

  it("leaves no stale refusal for the next Launch sheet", () => {
    useCockpitUiStore.setState({ launchRefusal: "old" });
    useCockpitUiStore.getState().openLaunch();
    expect(useCockpitUiStore.getState().launchRefusal).toBeNull();
    useCockpitUiStore.setState({ launchRefusal: "old", sheet: null });
    useCockpitUiStore.getState().openSheet("launch");
    expect(useCockpitUiStore.getState().launchRefusal).toBeNull();
  });
});

describe("the mission stall overlay", () => {
  const raw = JSON.parse(
    readFileSync(path.resolve(__dirname, "../../../../../crates/ados-control/tests/fixtures/pilot-frame-stall.json"), "utf8"),
  ) as Record<string, unknown>;
  const golden = parsePilotState(raw)!;
  const show = (p: PilotState) => useNavStreamStore.setState({ pilot: { data: p, at: Date.now() } });
  const button = (action: string) => document.querySelector<HTMLButtonElement>(`[data-testid=mission-stall-${action}]`);

  beforeEach(() => {
    pilotLink.calls.length = 0;
    pilotLink.result = { ok: true };
  });

  it("renders the golden stall, critical, with Skip waypoint, Return and Land here", async () => {
    show(golden);
    await mount();
    const el = document.querySelector("[data-testid=mission-stall-banner]");
    expect(el?.getAttribute("role")).toBe("alert");
    expect(el?.getAttribute("data-severity")).toBe("critical");
    expect(el?.textContent).toContain("No progress to waypoint 3 for 15 s · Skipping in 33 s");
    const buttons = [...(el?.querySelectorAll("button") ?? [])].map((b) => b.textContent);
    expect(buttons).toEqual(["Skip waypoint", "Return", "Land here"]);
    expect(alertBannerVisible(document, () => true)).toBe(true);
  });

  it("sends each action through the pilot link", async () => {
    show(golden);
    await mount();
    for (const action of ["mission_skip", "rth", "land"]) {
      await act(async () => button(action)?.click());
    }
    expect(pilotLink.calls).toEqual(["mission_skip", "rth", "land"]);
  });

  it("offers Land here only for a blocked return", async () => {
    show(parsePilotState({ ...raw, mission_stall: { kind: "return", seq: null, wp_label: null, since_ms: 1, skip_in_s: 40.2, window_s: 20 } })!);
    await mount();
    expect(document.querySelector("[data-testid=mission-stall-banner]")?.textContent).toContain("Return blocked · Landing in 41 s");
    expect(button("mission_skip")).toBeNull();
    expect(button("rth")).toBeNull();
    await act(async () => button("land")?.click());
    expect(pilotLink.calls).toEqual(["land"]);
  });

  it("shows a refused action on the ack line", async () => {
    pilotLink.result = { ok: false, err: "busy" };
    show(golden);
    await mount();
    await act(async () => button("mission_skip")?.click());
    expect(useConfirmStore.getState().ack).toEqual({ text: `Skip waypoint: ${refusalText("busy")}`, kind: "err" });
  });

  it("is gone once the stall clears", async () => {
    show({ ...golden, missionStall: null });
    await mount();
    expect(document.querySelector("[data-testid=mission-stall-banner]")).toBeNull();
  });
});
