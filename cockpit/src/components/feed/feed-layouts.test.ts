// @vitest-environment happy-dom
// Where the grids place the waiting-for-instructions prompt: never under
// another zone. Portrait stacks it in the normal flow between the follow strip
// and the left-column row; landscape puts it at the head of the alerts column
// under the strip, so the feed notices stack below it instead of over it.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { LandscapeGrid, PortraitGrid, type FeedSlots } from "@/components/feed/feed-layouts";
import { gridMeasures, Z } from "@/lib/cockpit-layout";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const m = gridMeasures("medium", false);
const mark = (id: string) => createElement("div", { "data-testid": id });

const slots: FeedSlots = {
  video: null,
  layers: null,
  hud: null,
  topBar: null,
  alerts: mark("notices"),
  overlays: mark("overlays"),
  map: null,
  cards: {},
  pip: null,
  leftColumn: () => mark("left-column"),
  rightColumn: () => null,
  bottomRow: null,
  followStrip: () => mark("strip"),
  followPrompt: mark("prompt"),
  skillPicker: mark("skill-picker"),
  ack: null,
  visionPanel: null,
  onscreenSticks: null,
};

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

const byId = (id: string) => host.querySelector<HTMLElement>(`[data-testid="${id}"]`);
const before = (a: HTMLElement, b: HTMLElement) => (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

/** The element and its ancestors up to the grid's own root. */
function chain(el: HTMLElement): HTMLElement[] {
  const out: HTMLElement[] = [];
  for (let e: HTMLElement | null = el; e && e !== host.firstElementChild; e = e.parentElement) out.push(e);
  return out;
}

describe("the waiting prompt's place in the grid", () => {
  it("portrait: in the flow between the follow strip and the left-column row", async () => {
    await act(async () => root.render(createElement(PortraitGrid, { slots })));
    const prompt = byId("prompt")!;
    expect(prompt).not.toBeNull();
    expect(before(byId("strip")!, prompt)).toBe(true);
    expect(before(prompt, byId("left-column")!)).toBe(true);
    // Not inside the strip's fixed-height box, not lifted out of the flow.
    expect(byId("strip")!.parentElement!.contains(prompt)).toBe(false);
    for (const e of chain(prompt)) expect(e.className).not.toMatch(/\babsolute\b/);
  });

  it("landscape: at the head of the alerts column, so a notice never covers it", async () => {
    await act(async () =>
      root.render(
        createElement(LandscapeGrid, {
          slots,
          m,
          input: { width: 1280, height: 720, m, widthClass: "medium", view: "pilot", flies: true, strip: true, gimbal: false },
        }),
      ),
    );
    const prompt = byId("prompt")!;
    expect(prompt).not.toBeNull();
    expect(prompt.closest('[data-zone="follow_strip"]')).toBeNull();
    const column = byId("notices")!.parentElement!;
    expect(column.className).toContain(Z.alerts);
    expect(prompt.parentElement).toBe(column);
    expect(column.firstElementChild).toBe(prompt);
    expect(before(prompt, byId("notices")!)).toBe(true);
  });
});
