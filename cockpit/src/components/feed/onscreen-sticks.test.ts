// @vitest-environment happy-dom
// The on-screen sticks draw the deadzone as a ring around each pad's centre,
// sized from the operator's deadzone setting on the knob's travel and drawn
// above the knob, so a thumb can see where the stick starts to fly.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";

import { STICK_KNOB_PX, ThumbPads } from "@/components/feed/onscreen-sticks";
import { STICK_PAD_PX } from "@/lib/cockpit-layout";
import { useControlsStore } from "@/stores/controls-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement | null = null;
let root: Root | null = null;

afterEach(async () => {
  if (root) await act(async () => root!.unmount());
  host?.remove();
  host = null;
  root = null;
});

async function ringWidths(deadzone: number): Promise<number[]> {
  useControlsStore.setState({ deadzone });
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root!.render(createElement(ThumbPads)));
  return (["l", "r"] as const).map((side) => {
    const ring = host!.querySelector<HTMLElement>(`[data-testid="onscreen-stick-deadzone-${side}"]`);
    expect(ring, `deadzone ring ${side}`).not.toBeNull();
    // Drawn after (above) the knob, so the knob never hides it.
    expect(ring!.nextElementSibling, `ring ${side} is the pad's last layer`).toBeNull();
    return parseFloat(ring!.style.width);
  });
}

describe("on-screen stick deadzone ring", () => {
  it("is the deadzone's share of the knob's travel on both sticks", async () => {
    for (const w of await ringWidths(0.08)) expect(w).toBeCloseTo(0.08 * (STICK_PAD_PX - STICK_KNOB_PX), 6);
  });

  it("follows the deadzone setting", async () => {
    for (const w of await ringWidths(0.2)) expect(w).toBeCloseTo(0.2 * (STICK_PAD_PX - STICK_KNOB_PX), 6);
  });
});
