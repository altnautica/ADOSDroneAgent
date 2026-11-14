import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import "@/shared/i18n";

import { SensorsView } from "@/components/panels/sensors-panel";
import type { SensorsStatus } from "@/shared/api/sensors";

type Status = Omit<SensorsStatus, "health"> & { health?: unknown };

const BASE: Status = { present: true, rig_id: null, calib_version: null, hub: null, fc: null, sensors: [], gnss: null };

const HEALTH = {
  flow: { state: "low", quality: 40, rate_hz: 19.8, age_ms: 50, mount: null },
  range: { state: "ok", height_m: 3.42, age_ms: 30, mount: { pos_m: [0, 0, 0.05] } },
  lidar: { state: "ok", rate_hz: 9.94, health: "good", age_ms: 80 },
  cameras: [
    { id: "front", leg: "main", role: "front", state: "live", fps: 29.7, age_ms: 33 },
    { id: "belly", leg: "belly", role: "down", state: "lost", fps: null, age_ms: null },
  ],
};

const render = (s: Status) => renderToStaticMarkup(createElement(SensorsView, { sensors: s as SensorsStatus, clock: null }));

describe("the sensors page's navigation sensor health", () => {
  it("shows flow, range, lidar and each camera as the spec words", () => {
    const out = render({ ...BASE, health: HEALTH });
    expect(out).toContain("Navigation sensors");
    for (const text of ["Flow low 40", "Range 3.4 m", "Lidar 9.9 Hz", "front 30 fps", "belly lost"]) expect(out).toContain(text);
    expect(out).toContain('style="color:#F59E0B"');
  });

  it("shows absent parts by their spec words", () => {
    const out = render({ ...BASE, health: { flow: { state: "absent" }, range: { state: "absent" }, lidar: { state: "absent" }, cameras: [] } });
    for (const text of ["No flow", "No range", "No lidar"]) expect(out).toContain(text);
  });

  it("shows null flow and range as unknown, never as no sensor", () => {
    const out = render({ ...BASE, health: { flow: null, range: null, lidar: { state: "ok", rate_hz: 9.94 }, cameras: [] } });
    expect(out).not.toContain("No flow");
    expect(out).not.toContain("No range");
    expect(out.match(/data-state="unknown"/g)?.length).toBe(2);
  });

  it("shows the health even when the sensor service is absent", () => {
    expect(render({ ...BASE, present: false, health: HEALTH })).toContain("Lidar 9.9 Hz");
  });

  it("is hidden when the agent sends no health", () => {
    expect(render(BASE)).not.toContain("Navigation sensors");
  });
});
