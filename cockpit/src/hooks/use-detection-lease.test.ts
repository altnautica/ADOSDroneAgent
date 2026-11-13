// @vitest-environment happy-dom
// The detection lease a target overlay holds lets go while the page is hidden
// and is taken again when it shows, like the view and demand leases.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useDetectionLease } from "@/hooks/use-detection-lease";
import { LEASE_HOLD_S, LEASE_RENEW_MS } from "@/lib/detection-lease";

const sent: { camera: string; holdS: number }[] = [];

vi.mock("@/lib/api", () => ({
  requestDetection: async (camera: string, holdS: number) => {
    sent.push({ camera, holdS });
    return { detecting: true };
  },
}));

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

function setHidden(hidden: boolean): void {
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => (hidden ? "hidden" : "visible") });
  document.dispatchEvent(new Event("visibilitychange"));
}

afterEach(() => {
  vi.useRealTimers();
  setHidden(false);
  sent.length = 0;
});

describe("the detection lease", () => {
  it("is released while the page is hidden and taken again when it shows", async () => {
    vi.useFakeTimers();
    const Overlay = () => {
      useDetectionLease("main", true);
      return null;
    };
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(createElement(Overlay)));
    expect(sent).toEqual([{ camera: "main", holdS: LEASE_HOLD_S }]);

    await act(async () => setHidden(true));
    expect(sent.at(-1)).toEqual({ camera: "main", holdS: 0 });
    const atHide = sent.length;
    // Nothing is renewed while hidden.
    await act(async () => vi.advanceTimersByTimeAsync(LEASE_RENEW_MS * 5));
    expect(sent.length).toBe(atHide);

    await act(async () => setHidden(false));
    expect(sent.at(-1)).toEqual({ camera: "main", holdS: LEASE_HOLD_S });
    await act(async () => root.unmount());
    expect(sent.at(-1)).toEqual({ camera: "main", holdS: 0 });
  });
});
