// @vitest-environment happy-dom
// The sensor words as the Feed renders them: the lidar word in the obstacle
// radar's header, the flow and range words beside the bottom row's AGL, and a
// stale camera's word on its Menu camera button.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";

import { BottomRow } from "@/components/feed/cockpit/cockpit-zones";
import { ObstacleRadarCard } from "@/components/feed/cockpit/radar-cards";
import { CameraSwitcher } from "@/components/feed/stream-tabs";
import { useCameraHealthStore } from "@/hooks/use-camera-health";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { parseCameraHealth, type CameraHealth } from "@/shared/hud/sensor-health";
import { useNavStreamStore } from "@/stores/nav-stream-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

async function render(el: Parameters<Root["render"]>[0]): Promise<HTMLElement> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(el));
  return host;
}

function setPilot(sensors: unknown, extra: Record<string, unknown> = {}): void {
  const data = { owner: "idle", readiness: [], planNed: [], aglM: 3.4, aglSource: "rangefinder", sensors, ...extra };
  useNavStreamStore.setState({ pilot: { data: data as unknown as PilotState, at: Date.now() - 100 } });
}

const SENSORS = {
  flow: { state: "low", quality: 40, rate_hz: 20, age_ms: 40 },
  range: { state: "ok", height_m: 3.42, age_ms: 30 },
  lidar: { state: "degraded", rate_hz: 5.24, health: "warning", age_ms: 120 },
};

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  useNavStreamStore.getState().clear();
  useCameraHealthStore.setState({ cameras: [] });
});

describe("the obstacle radar's header", () => {
  it("shows the lidar's scan rate from sensors.lidar in its state colour", async () => {
    setPilot(SENSORS);
    const el = (await render(createElement(ObstacleRadarCard))).querySelector('[data-testid="radar-lidar"]') as HTMLElement;
    expect(el.textContent).toBe("Lidar 5.2 Hz");
    expect(el.dataset.state).toBe("degraded");
    expect(el.style.color).toMatch(/#F59E0B|rgb\(245, 158, 11\)/i);
  });

  it("reads No lidar when the pilot reports none", async () => {
    setPilot({ ...SENSORS, lidar: { state: "absent" } });
    const el = (await render(createElement(ObstacleRadarCard))).querySelector('[data-testid="radar-lidar"]');
    expect(el?.textContent).toBe("No lidar");
  });

  it("shows no lidar word without a sensors block", async () => {
    setPilot(null);
    const host = await render(createElement(ObstacleRadarCard));
    expect(host.querySelector('[data-testid="radar-lidar"]')).toBeNull();
  });
});

describe("the bottom row's sensor words", () => {
  it("shows flow then range beside AGL", async () => {
    setPilot(SENSORS);
    const words = (await render(createElement(BottomRow))).querySelector('[data-testid="sensor-words"]');
    expect([...(words?.children ?? [])].map((c) => c.textContent)).toEqual(["Flow low 40", "Range 3.4 m"]);
  });

  it("leaves absent parts out", async () => {
    setPilot({ ...SENSORS, flow: { state: "absent" }, range: { state: "absent" } });
    expect((await render(createElement(BottomRow))).querySelector('[data-testid="sensor-words"]')).toBeNull();
  });
});

describe("the Menu's camera buttons", () => {
  it("a stale camera reads its state word; a live one its label", async () => {
    useCameraHealthStore.setState({
      cameras: [
        { id: "front", leg: "main", role: "front", state: "live", fps: 30, age_ms: 30 },
        { id: "belly", leg: "belly", role: "down", state: "lost", fps: null, age_ms: null },
      ].map(parseCameraHealth) as CameraHealth[],
    });
    const host = await render(createElement(CameraSwitcher, { cameras: [{ id: "main", role: "front" }, { id: "belly", role: "down" }] }));
    expect([...host.querySelectorAll("button")].map((b) => b.textContent)).toEqual(["Front", "Belly lost"]);
  });
});
