import { describe, expect, it } from "vitest";

import { compactCount, mapStatusRows } from "@/lib/map-status-view";
import type { MapStatus } from "@/shared/hud/nav-types";

const EMPTY: MapStatus = {
  sessionId: null,
  placement: null,
  memoryUsedMb: null,
  memoryBudgetMb: null,
  voxelM: null,
  keyframesDropped: null,
  sweepPoints: null,
  areaM2: null,
  navPose: null,
  upload: null,
  stream: null,
};

const row = (s: MapStatus, id: string) => mapStatusRows(s).find((r) => r.id === id);

describe("mapStatusRows", () => {
  it("shows dashes, never zeros, for absent fields", () => {
    const rows = mapStatusRows(EMPTY);
    expect(rows.map((r) => r.id)).toEqual(["placement", "memory", "voxel", "keyframes", "points", "area", "session"]);
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

  it("warns on dropped keyframes", () => {
    expect(row({ ...EMPTY, keyframesDropped: 0 }, "keyframes")?.tone).toBe("normal");
    expect(row({ ...EMPTY, keyframesDropped: 3 }, "keyframes")).toMatchObject({ value: "3", tone: "warn" });
  });

  it("shows upload progress and failures", () => {
    const up = row({ ...EMPTY, upload: { state: "uploading", bytesDone: 12e6, bytesTotal: 48e6 } }, "upload");
    expect(up).toMatchObject({ value: "Uploading 25 % · 12.0 / 48.0 MB", fraction: 0.25 });
    expect(row({ ...EMPTY, upload: { state: "failed", bytesDone: 0, bytesTotal: 0 } }, "upload")).toMatchObject({
      value: "Failed",
      tone: "crit",
    });
  });

  it("shows the live stream rate", () => {
    expect(row({ ...EMPTY, stream: { state: "live", bytesPerS: 1_000_000 } }, "stream")).toMatchObject({
      value: "Live · 8.0 Mbit/s",
      tone: "good",
    });
    expect(row({ ...EMPTY, stream: { state: "off", bytesPerS: 0 } }, "stream")?.value).toBe("Off");
  });
});

describe("compactCount", () => {
  it("abbreviates thousands and millions", () => {
    expect(compactCount(950)).toBe("950");
    expect(compactCount(48_500)).toBe("48.5 k");
  });
});
