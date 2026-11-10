// @vitest-environment happy-dom
// The controller tour card as the real component places it: it mounts at app
// level, so a top bar that appears or grows after it must move it clear.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { ControllerTour } from "@/components/controls/controller-tour";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore } from "@/stores/input-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

/** Laid-out boxes by `data-zone` (happy-dom has no layout); the card is
 *  200 px tall. */
const boxes: Record<string, { x: number; y: number; w: number; h: number }> = {};
const CARD_H = 200;

function rectOf(el: Element): DOMRect {
  const zone = el.getAttribute("data-zone");
  const b = zone ? boxes[zone] : el.getAttribute("data-testid") === "controls-tour" ? { x: 0, y: 0, w: 360, h: CARD_H } : null;
  const r = b ?? { x: 0, y: 0, w: 0, h: 0 };
  return { x: r.x, y: r.y, left: r.x, top: r.y, width: r.w, height: r.h, right: r.x + r.w, bottom: r.y + r.h, toJSON: () => r } as DOMRect;
}

const realRect = HTMLElement.prototype.getBoundingClientRect;
let host: HTMLDivElement;
let root: Root;

/** Let the observers and their animation frame run: happy-dom delivers
 *  MutationObserver records and animation frames on its own real-time queue,
 *  which fake timers do not drive. */
async function settle(): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 50));
  });
}

function card(): HTMLElement {
  return document.querySelector<HTMLElement>('[data-testid="controls-tour"]')!;
}

beforeEach(async () => {
  for (const k of Object.keys(boxes)) delete boxes[k];
  HTMLElement.prototype.getBoundingClientRect = function () {
    return rectOf(this);
  };
  Object.assign(window, { innerWidth: 580, innerHeight: 360 });
  useControlsStore.setState({ tourSeen: false });
  useInputStore.setState({ pad: null, padAnnounced: true });
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(ControllerTour)));
  await settle();
});

afterEach(async () => {
  await act(async () => root.unmount());
  document.body.innerHTML = "";
  HTMLElement.prototype.getBoundingClientRect = realRect;
});

describe("the tour card's placement", () => {
  it("moves clear of a top bar that mounts after it", async () => {
    // No top bar yet: the card sits as high as the window lets it.
    expect(parseFloat(card().style.top)).toBeLessThan(52);
    boxes.top_bar = { x: 0, y: 0, w: 580, h: 52 };
    const bar = document.createElement("div");
    bar.setAttribute("data-zone", "top_bar");
    document.body.appendChild(bar);
    await settle();
    expect(parseFloat(card().style.top)).toBeGreaterThanOrEqual(52);
  });

  it("follows a screen change that swaps the top bar for a taller one", async () => {
    boxes.top_bar = { x: 0, y: 0, w: 580, h: 40 };
    const bar = document.createElement("div");
    bar.setAttribute("data-zone", "top_bar");
    document.body.appendChild(bar);
    await settle();
    expect(parseFloat(card().style.top)).toBeGreaterThanOrEqual(40);
    bar.remove();
    boxes.top_bar = { x: 0, y: 0, w: 580, h: 90 };
    const taller = document.createElement("div");
    taller.setAttribute("data-zone", "top_bar");
    document.body.appendChild(taller);
    await settle();
    const top = parseFloat(card().style.top);
    // Never over the bar: the card's top sits below its bottom edge.
    expect(top).toBeGreaterThanOrEqual(90);
  });
});

describe("the tour card's observer", () => {
  it("ignores style and class changes elsewhere, which a HUD makes every frame", async () => {
    boxes.top_bar = { x: 0, y: 0, w: 580, h: 52 };
    const bar = document.createElement("div");
    bar.setAttribute("data-zone", "top_bar");
    document.body.appendChild(bar);
    const tape = document.createElement("div");
    document.body.appendChild(tape);
    await settle();
    let reads = 0;
    HTMLElement.prototype.getBoundingClientRect = function () {
      reads += 1;
      return rectOf(this);
    };
    for (let i = 0; i < 5; i++) {
      tape.style.transform = `translateY(${i}px)`;
      tape.className = `tape-${i}`;
      await settle();
    }
    expect(reads).toBe(0);
    // A zone coming or going still re-places it.
    const alerts = document.createElement("div");
    alerts.setAttribute("data-zone", "alert_stack");
    document.body.appendChild(alerts);
    await settle();
    expect(reads).toBeGreaterThan(0);
  });
});
