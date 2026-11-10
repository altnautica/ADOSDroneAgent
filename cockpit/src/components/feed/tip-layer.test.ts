// @vitest-environment happy-dom
// The tip's "Got it" button: a click and the keyboard (Enter, Space) both
// dismiss the tip and mark it seen. An unknown arm state shows no tip.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { TipLayer } from "@/components/feed/tip-layer";
import tipsJson from "@/shared/cockpit-spec/tips.json";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useTipsStore } from "@/stores/tips-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const ANCHOR_ATTRS: [string, string][] = [
  ["data-zone", "status_pill"],
  ["data-testid", "flight-state-chip"],
  ["data-zone", "layer_button"],
  ["data-zone", "stop"],
  ["data-testid", "skill-picker-chip"],
  ["data-zone", "bottom_left_card"],
  ["data-zone", "gimbal_pad"],
];

let host: HTMLDivElement;
let root: Root;

beforeEach(async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "setTimeout", "clearTimeout", "Date"] });
  localStorage.clear();
  useTipsStore.getState().reload();
  useNavStore.setState({ activeTabId: "feed", detailStack: [], quickMenuOpen: false });
  // The flight controller reports the vehicle disarmed (no pilot frame).
  useFlightStore.setState({ telemetry: { armed: false } as never });
  // Every anchor laid out and on screen.
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue(new DOMRect(400, 300, 80, 40));
  for (const [k, v] of ANCHOR_ATTRS) {
    const el = document.createElement("div");
    el.setAttribute(k, v);
    document.body.appendChild(el);
  }
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(TipLayer, { gimbalAvailable: false, scale: 1 })));
  await act(async () => vi.advanceTimersByTime(tipsJson.rules.first_delay_ms + 500));
});

afterEach(async () => {
  await act(async () => root.unmount());
  document.body.innerHTML = "";
  vi.restoreAllMocks();
  vi.useRealTimers();
});

function shownTip(): string {
  const tip = document.querySelector("[data-testid=cockpit-tip]");
  expect(tip).not.toBeNull();
  return tip?.getAttribute("data-tip") ?? "";
}

const button = () => document.querySelector<HTMLButtonElement>("[data-testid=cockpit-tip-dismiss]");

describe("the tip's Got it button", () => {
  it("dismisses on a click and marks the tip seen", async () => {
    const id = shownTip();
    await act(async () => button()?.click());
    expect(document.querySelector("[data-testid=cockpit-tip]")).toBeNull();
    expect(useTipsStore.getState().tips.seen).toContain(id);
  });

  for (const key of ["Enter", " "]) {
    it(`dismisses on the ${key === " " ? "Space" : key} key and marks the tip seen`, async () => {
      const id = shownTip();
      await act(async () => {
        button()?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
      });
      expect(document.querySelector("[data-testid=cockpit-tip]")).toBeNull();
      expect(useTipsStore.getState().tips.seen).toContain(id);
    });
  }
});

describe("an unknown arm state", () => {
  it("hides the tip without marking it seen and shows none while unknown", async () => {
    const id = shownTip();
    await act(async () => useFlightStore.setState({ telemetry: {} as never }));
    await act(async () => vi.advanceTimersByTime(tipsJson.rules.first_delay_ms + tipsJson.rules.gap_between_ms + 500));
    expect(document.querySelector("[data-testid=cockpit-tip]")).toBeNull();
    expect(useTipsStore.getState().tips.seen).not.toContain(id);
    await act(async () => useFlightStore.setState({ telemetry: null }));
    await act(async () => vi.advanceTimersByTime(1000));
    expect(document.querySelector("[data-testid=cockpit-tip]")).toBeNull();
  });
});
