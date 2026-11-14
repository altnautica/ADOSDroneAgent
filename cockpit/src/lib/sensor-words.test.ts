import { describe, expect, it } from "vitest";

import { freshSensors, legCameraWord, radarLidarWord } from "@/lib/sensor-words";
import { CHIP_STALE_AFTER_MS } from "@/shared/hud/flight-state";
import { parseCameraHealth, type CameraHealth } from "@/shared/hud/sensor-health";

const SENSORS = {
  flow: { state: "ok", quality: 212, rate_hz: 20, age_ms: 40 },
  range: { state: "ok", height_m: 3.42, age_ms: 30 },
  lidar: { state: "ok", rate_hz: 9.94, health: "good", age_ms: 80 },
};
const pilot = (sensors: unknown, at = 1000) => ({ data: { sensors }, at });

describe("the radar's lidar header", () => {
  it("reads the lidar's scan rate from the pilot's sensors.lidar", () => {
    expect(radarLidarWord(pilot(SENSORS), 1200)).toMatchObject({ text: "Lidar 9.9 Hz", color: "ready", hex: "#00C853" });
  });

  it("reads lost and absent in their state colours", () => {
    expect(radarLidarWord(pilot({ ...SENSORS, lidar: { state: "lost" } }), 1200)).toMatchObject({ text: "Lidar lost", color: "not_ready" });
    expect(radarLidarWord(pilot({ ...SENSORS, lidar: { state: "absent" } }), 1200)).toMatchObject({ text: "No lidar", color: "stale" });
  });

  it("shows nothing without sensors or once the pilot frame is stale", () => {
    expect(radarLidarWord(pilot(null), 1200)).toBeNull();
    expect(radarLidarWord(null, 1200)).toBeNull();
    expect(radarLidarWord(pilot(SENSORS), 1000 + CHIP_STALE_AFTER_MS)).toBeNull();
    expect(freshSensors(pilot(SENSORS), 1000 + CHIP_STALE_AFTER_MS - 1)?.range.heightM).toBe(3.42);
  });
});

describe("a stream leg's camera word", () => {
  const cams = [
    { id: "front", leg: "main", role: "front", state: "live", fps: 30, age_ms: 30 },
    { id: "belly", leg: "belly", role: "down", state: "stale", fps: 2, age_ms: 1800 },
  ].map(parseCameraHealth) as CameraHealth[];

  it("names a stale leg by its label and adds nothing for a live one", () => {
    expect(legCameraWord(cams, { id: "belly", role: "down" }, "Belly")?.text).toBe("Belly stale");
    expect(legCameraWord(cams, { id: "main", role: "front" }, "Front")).toBeNull();
    expect(legCameraWord([], { id: "main" }, "Front")).toBeNull();
  });
});
