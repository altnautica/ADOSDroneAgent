import { beforeEach, describe, expect, it, vi } from "vitest";

const apiFetch = vi.fn();
vi.mock("../api-fetch", () => ({ apiFetch: (...args: unknown[]) => apiFetch(...args) }));

import spec from "../cockpit-spec/sensor-health.json";
import {
  CAMERA_STATES,
  FLOW_STATES,
  formatSigma,
  LIDAR_STATES,
  postRigLidar,
  RANGE_STATES,
  type SensorHealth,
} from "./sensors";

describe("formatSigma", () => {
  it("prints nanoseconds below one microsecond", () => {
    expect(formatSigma(420)).toBe("420 ns");
    expect(formatSigma(0)).toBe("0 ns");
  });

  it("prints microseconds and milliseconds above that", () => {
    expect(formatSigma(1500)).toBe("2 µs");
    expect(formatSigma(2_500_000)).toBe("2.50 ms");
  });

  it("prints a dash for an unknown value", () => {
    expect(formatSigma(-1)).toBe("—");
  });
});

describe("sensor health", () => {
  it("names exactly the state words the spec classifies by", () => {
    const keys = (o: object) => Object.keys(o).sort();
    expect([...FLOW_STATES].sort()).toEqual(keys(spec.states.flow));
    expect([...RANGE_STATES].sort()).toEqual(keys(spec.states.range));
    expect([...LIDAR_STATES].sort()).toEqual(keys(spec.states.lidar));
    expect([...CAMERA_STATES].sort()).toEqual(keys(spec.states.camera));
  });

  it("types the agent's health block", () => {
    const health: SensorHealth = {
      flow: {
        state: "low",
        quality: 90,
        rate_hz: 19.6,
        age_ms: 130,
        mount: { yaw_cdeg: 9000, pos_m: [0.05, 0, 0.03] },
      },
      range: { state: "ok", height_m: 1.42, age_ms: 125, mount: null },
      lidar: { state: "ok", rate_hz: 9.9, health: "good", age_ms: 80 },
      cameras: [{ id: "front", role: "primary", leg: "main", state: "live", fps: 29.7, age_ms: 40 }],
    };
    expect(health.cameras[0].leg).toBe("main");
  });
});

describe("postRigLidar", () => {
  beforeEach(() => {
    apiFetch.mockReset();
    apiFetch.mockResolvedValue({ ok: true, rig_id: "r", calib_version: "uncalibrated", sensor: {} });
  });

  it("posts to the rig lidar route with no body", async () => {
    const res = await postRigLidar();
    expect(apiFetch).toHaveBeenCalledWith("/api/v1/sensors/rig/lidar", { method: "POST" });
    expect(res.calib_version).toBe("uncalibrated");
  });
});
