// @vitest-environment happy-dom
// The top bar as it renders: the link cell reads the newest vehicle frame the
// cockpit holds, so a ground station (whose status carries no heartbeat age)
// with fresh pilot frames reads a live link.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";

import { TopBar } from "@/components/feed/cockpit/top-bar";
import { linkCell, TOP_BAR_LINK } from "@/lib/flight-layout";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { INITIAL_FLIGHT_STATE, useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useStatusStore } from "@/stores/status-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

async function render(): Promise<HTMLElement> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(TopBar)));
  return host.querySelector('[data-zone="link"]') as HTMLElement;
}

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  useNavStreamStore.getState().clear();
  useFlightStore.setState({ ...INITIAL_FLIGHT_STATE });
  useStatusStore.setState({ status: null });
});

describe("the top bar's link cell", () => {
  it("a ground station with fresh pilot frames and no heartbeat age reads a live link", async () => {
    // A ground station's status carries no heartbeat age; none is set here.
    const data = { owner: "idle", controlMode: "ados", profile: "normal", avoid: "brake", armed: false, landed: true, vehicleClass: "multirotor", readiness: [], planNed: [] };
    useNavStreamStore.setState({ pilot: { data: data as unknown as PilotState, at: Date.now() - 300 } });
    const link = await render();
    expect(link.dataset.severity).toBe("ok");
    expect(link.textContent).not.toContain(linkCell(null).text);
  });

  it("fresh live telemetry alone reads a live link", async () => {
    useFlightStore.setState({ lastLiveAt: performance.now() - 200 });
    const link = await render();
    expect(link.dataset.severity).toBe("ok");
  });

  it("with no frame and no heartbeat it reads no link", async () => {
    const link = await render();
    expect(link.dataset.severity).toBe("critical");
    expect(link.textContent).toContain(linkCell(null).text);
  });
});

describe("link age format", () => {
  it("reproduces the spec's golden rows", () => {
    for (const [ms, text] of TOP_BAR_LINK.age_format.cases) {
      expect(linkCell(ms).text).toBe(text);
    }
  });
});
