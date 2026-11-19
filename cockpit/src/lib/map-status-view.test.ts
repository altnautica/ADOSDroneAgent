import { describe, expect, it } from "vitest";

import { compactCount, mapStatusRows } from "@/lib/map-status-view";
import type { MapStatus } from "@/shared/hud/nav-types";

const EMPTY: MapStatus = {
  placement: null,
  memoryUsedMb: null,
  memoryBudgetMb: null,
  voxelM: null,
  sweepPoints: null,
  areaM2: null,
  poseProvider: null,
  imuFeed: null,
};

const row = (s: MapStatus, id: string) => mapStatusRows(s).find((r) => r.id === id);

describe("mapStatusRows", () => {
  it("shows dashes, never zeros, for absent fields", () => {
    const rows = mapStatusRows(EMPTY);
    expect(rows.map((r) => r.id)).toEqual(["placement", "memory", "voxel", "points", "area", "pose"]);
    expect(rows.every((r) => r.value === "—" && r.tone === "muted")).toBe(true);
  });

  it("formats placement, voxel, points and area", () => {
    const s = { ...EMPTY, placement: "hybrid" as const, voxelM: 0.1, sweepPoints: 1_234_567, areaM2: 1240.4 };
    expect(row(s, "placement")?.value).toBe("Hybrid");
    expect(row(s, "voxel")?.value).toBe("10 cm");
    expect(row(s, "points")?.value).toBe("1.2 M");
    expect(row(s, "area")?.value).toBe("1,240 m²");
  });

  it("warns and goes critical as memory nears the budget", () => {
    expect(row({ ...EMPTY, memoryUsedMb: 300, memoryBudgetMb: 614 }, "memory")).toMatchObject({
      value: "300 / 614 MB · 49 %",
      tone: "normal",
    });
    expect(row({ ...EMPTY, memoryUsedMb: 540, memoryBudgetMb: 614 }, "memory")?.tone).toBe("warn");
    expect(row({ ...EMPTY, memoryUsedMb: 600, memoryBudgetMb: 614 }, "memory")?.tone).toBe("crit");
    expect(row({ ...EMPTY, memoryUsedMb: 700, memoryBudgetMb: 614 }, "memory")?.fraction).toBe(1);
    expect(row({ ...EMPTY, memoryUsedMb: 300 }, "memory")?.value).toBe("300 MB");
  });

  it("names the pose provider", () => {
    expect(row({ ...EMPTY, poseProvider: "lidar_planar" }, "pose")).toMatchObject({ value: "Planar LiDAR", tone: "normal" });
  });

  it("shows the FC IMU feed only when the pose takes it, toned by its state", () => {
    expect(row({ ...EMPTY, imuFeed: { state: "ok", reason: null, rateHz: 49.6 } }, "imu")).toMatchObject({
      value: "OK · 50 Hz",
      tone: "good",
    });
    expect(row({ ...EMPTY, imuFeed: { state: "degraded", reason: "starved", rateHz: 8 } }, "imu")?.tone).toBe("warn");
    expect(row({ ...EMPTY, imuFeed: { state: "lost", reason: "stalled", rateHz: null } }, "imu")).toMatchObject({
      value: "Lost",
      tone: "crit",
    });
  });
});

describe("compactCount", () => {
  it("abbreviates thousands and millions", () => {
    expect(compactCount(950)).toBe("950");
    expect(compactCount(48_500)).toBe("48.5 k");
  });
});
