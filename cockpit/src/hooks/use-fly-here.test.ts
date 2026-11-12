// @vitest-environment happy-dom
// The map long-press: a press that outlives its map does nothing.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useFlyHere, type MapPressHandlers } from "@/hooks/use-fly-here";
import { FLY_HERE_PRESS_MS } from "@/lib/fly-here";
import { onToast, type ToastMessage } from "@/shared/api/toast";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

afterEach(() => {
  vi.useRealTimers();
  useNavStreamStore.setState({ pilot: null });
});

describe("the map long-press", () => {
  it("opens nothing once its map has unmounted", async () => {
    vi.useFakeTimers();
    const landed = { armed: false, landed: true } as unknown as PilotState;
    useNavStreamStore.setState({ pilot: { data: landed, at: Date.now() } });
    useCockpitUiStore.setState({ sheet: null });
    let handlers: MapPressHandlers | null = null;
    const Map = () => {
      handlers = useFlyHere(() => ({ ll: null, fromDrone: { n: 10, e: 5 } }), null).handlers;
      return null;
    };
    const host = document.createElement("div");
    const root = createRoot(host);
    await act(async () => root.render(createElement(Map)));
    expect(handlers).not.toBeNull();
    const down = { pointerType: "touch", button: 0, clientX: 50, clientY: 60 };
    act(() => handlers?.onPointerDown(down as never));
    await act(async () => root.unmount());
    vi.advanceTimersByTime(FLY_HERE_PRESS_MS + 50);
    expect(useCockpitUiStore.getState().sheet).toBeNull();
  });

  it("says the flight controller link is down when the pilot has no arm state", async () => {
    vi.useFakeTimers();
    const noFc = {
      controlMode: "ados",
      surfaceOps: ["goto"],
      armed: null,
      landed: null,
      fcFailsafe: false,
    } as unknown as PilotState;
    useNavStreamStore.setState({ pilot: { data: noFc, at: Date.now() } });
    useCockpitUiStore.setState({ sheet: null });
    const seen: ToastMessage[] = [];
    const off = onToast((t) => seen.push(t));
    let handlers: MapPressHandlers | null = null;
    const Map = () => {
      handlers = useFlyHere(() => ({ ll: { lat: 1, lon: 2 }, fromDrone: { n: 10, e: 5 } }), null).handlers;
      return null;
    };
    const host = document.createElement("div");
    const root = createRoot(host);
    await act(async () => root.render(createElement(Map)));
    expect(handlers).not.toBeNull();
    act(() => handlers?.onPointerDown({ pointerType: "touch", button: 0, clientX: 50, clientY: 60 } as never));
    act(() => {
      vi.advanceTimersByTime(FLY_HERE_PRESS_MS + 50);
    });
    expect(seen.map((t) => t.message)).toEqual(["No flight controller link"]);
    expect(useCockpitUiStore.getState().sheet).toBeNull();
    off();
    await act(async () => root.unmount());
  });
});
