import { readFileSync } from "node:fs";
import path from "node:path";

import type { TFunction } from "i18next";
import { describe, expect, it } from "vitest";

import { cellularCapDetail, moveEntry } from "./uplink-view";

const t = ((key: string, opts?: Record<string, unknown>) =>
  opts ? `${key}:${JSON.stringify(opts)}` : key) as unknown as TFunction;
const panel = readFileSync(path.resolve(__dirname, "../components/network/network-uplink-panel.tsx"), "utf8");

describe("uplink matrix", () => {
  it("labels the cellular percent as data-cap use", () => {
    expect(cellularCapDetail(12.4, t)).toBe('uplink.state.capUsed:{"percent":"12"}');
    expect(cellularCapDetail(undefined, t)).toBeNull();
  });

  it("moves a failover entry within range only", () => {
    expect(moveEntry(["a", "b", "c"], 1, -1)).toEqual(["b", "a", "c"]);
    expect(moveEntry(["a", "b"], 0, -1)).toBeNull();
  });

  it("reorder and share-uplink wait for a confirm", () => {
    expect(panel).toMatch(/onClick=\{\(\) => askMove\(idx, -1\)\}/);
    expect(panel).toMatch(/onCheckedChange=\{\(v\) => setPending\(\{ kind: "share", enabled: v \}\)\}/);
    expect(panel).not.toMatch(/onClick=\{\(\) => void onMovePriority/);
  });

  it("marks the matrix stale and drops the active badge after a failed read", () => {
    expect(panel).toMatch(/const stale = loadFailed && net != null/);
    expect(panel).toMatch(/\{active && !stale && \(/);
    expect(panel).toMatch(/stale && "opacity-50"/);
  });

  it("gives the order arrows a 44 px target on a phone", () => {
    expect(panel).toMatch(/max-md:h-11 max-md:w-11/);
    expect(panel).toMatch(/COARSE_HIT_AREA/);
  });
});
