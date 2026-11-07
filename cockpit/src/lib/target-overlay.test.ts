import { describe, expect, it } from "vitest";

import type { InstalledExtension, TargetAction } from "@/lib/extensions";
import {
  designatedFor,
  isDesignated,
  isVideoLost,
  nextGhost,
  nextLastFollowRect,
  targetInputFor,
  targetMenuEntries,
} from "@/lib/target-overlay";
import type { DrawItem, ResolvedTargets, TargetDetection } from "@/shared/hud/target-state";
import type { CockpitDetectionBatch } from "@/stores/detections-store";

const box = { x: 100, y: 50, width: 40, height: 80 };
const det: TargetDetection = { bbox: box, classLabel: "person", confidence: 0.9, trackId: 12, lockState: "locked" };

const batch: CockpitDetectionBatch = {
  modelId: "m",
  cameraId: "belly",
  frameId: 7,
  tsMs: 1,
  frameWidth: 1280,
  frameHeight: 720,
  detections: [det],
  receivedAt: 1000,
};

function item(kind: DrawItem["kind"], rect: DrawItem["rect"], trackId: number | null = 12): DrawItem {
  return {
    kind,
    key: `${kind}-${trackId}`,
    trackId,
    rect,
    topTag: null,
    bottomTags: [],
    modifiers: { uncertain: false, followStale: false, reacquired: false },
    reticle: false,
    feetTick: false,
    confidence: 0.9,
  };
}
const resolved = (...items: DrawItem[]): ResolvedTargets => ({ frameW: 1280, frameH: 720, items });

describe("targetInputFor", () => {
  it("multi-stream compares against the video's camera", () => {
    const r = targetInputFor(true, "main", batch);
    expect(r.cameraId).toBe("main");
    expect(r.batch).toEqual({ cameraId: "belly", frameWidth: 1280, frameHeight: 720, detections: [det] });
  });

  it("single-stream draws its newest batch whatever camera it carries", () => {
    expect(targetInputFor(false, "main", batch).cameraId).toBe("belly");
    expect(targetInputFor(false, "main", null)).toEqual({ cameraId: "main", batch: null });
  });
});

describe("isVideoLost", () => {
  it("is lost unless live, except on the demo node", () => {
    expect(isVideoLost("live", false)).toBe(false);
    expect(isVideoLost("connecting", false)).toBe(true);
    expect(isVideoLost("failed", true)).toBe(false);
  });
});

describe("nextLastFollowRect", () => {
  it("refreshes from the followed item and keeps the last one otherwise", () => {
    const rect = { x: 1, y: 2, w: 3, h: 4 };
    const first = nextLastFollowRect(null, resolved(item("FOLLOWING", rect)), 500);
    expect(first).toEqual({ trackId: 12, rect, frameW: 1280, frameH: 720, at: 500 });
    expect(nextLastFollowRect(first, resolved(item("TRACKED", rect)), 900)).toBe(first);
  });
});

describe("nextGhost", () => {
  it("samples the FOLLOWING box's bottom-centre", () => {
    const g = nextGhost([], resolved(item("FOLLOWING", { x: 100, y: 50, w: 40, h: 80 })), 1000);
    expect(g).toEqual([{ t: 1000, x: 120, y: 130 }]);
  });

  it("keeps the path unchanged while coasting", () => {
    const prev = [{ t: 1000, x: 120, y: 130 }];
    expect(nextGhost(prev, resolved(item("COASTING", { x: 0, y: 0, w: 1, h: 1 })), 1100)).toBe(prev);
  });
});

describe("designation", () => {
  it("designates a picked box with its track", () => {
    expect(designatedFor("main", det)).toEqual({ cameraId: "main", trackId: 12, bbox: box });
    expect(designatedFor("main", { ...det, bbox: undefined })).toBeNull();
  });

  it("matches by track id, else by exact box, on the same camera", () => {
    const ref = designatedFor("main", det);
    expect(isDesignated(ref, "main", det)).toBe(true);
    expect(isDesignated(ref, "belly", det)).toBe(false);
    expect(isDesignated(ref, "main", { ...det, trackId: 13 })).toBe(false);
    const boxRef = { cameraId: "main", trackId: null, bbox: box };
    expect(isDesignated(boxRef, "main", { ...det, trackId: null })).toBe(true);
    expect(isDesignated(boxRef, "main", { ...det, trackId: 12 })).toBe(false);
    expect(isDesignated(null, "main", det)).toBe(false);
  });
});

describe("targetMenuEntries", () => {
  const action = (over: Partial<TargetAction>): TargetAction => ({
    pluginId: "com.altnautica.follow-me",
    id: "follow",
    label: "Follow",
    order: 0,
    appliesToClass: null,
    designate: true,
    configKey: "active",
    configValue: true,
    ...over,
  });
  const ext = (actions: TargetAction[]): InstalledExtension => ({
    pluginId: "com.altnautica.follow-me",
    name: "Follow",
    entrypoint: null,
    isolation: "iframe",
    grantedCapabilities: [],
    panels: [],
    skills: [],
    rejected: [],
    locales: [],
    targetActions: actions,
  });

  it("offers Lock, or Release on the designated target", () => {
    expect(targetMenuEntries(det, false, [], () => true)).toEqual([{ kind: "lock" }]);
    expect(targetMenuEntries(det, true, [], () => true)).toEqual([{ kind: "release" }]);
  });

  it("filters plugin actions by class, case-insensitively", () => {
    const a = action({ appliesToClass: "PERSON" });
    const b = action({ id: "car", appliesToClass: "car", designate: false });
    expect(targetMenuEntries(det, false, [ext([a, b])], () => true)).toEqual([{ kind: "lock" }, { kind: "plugin", action: a }]);
  });

  it("hides a designating action while its skill is not drivable", () => {
    const a = action({});
    const plain = action({ id: "tag", designate: false });
    const seen: [string, string | null][] = [];
    const entries = targetMenuEntries(det, false, [ext([a, plain])], (p, k) => {
      seen.push([p, k]);
      return false;
    });
    expect(entries).toEqual([{ kind: "lock" }, { kind: "plugin", action: plain }]);
    expect(seen).toEqual([["com.altnautica.follow-me", "active"]]);
  });
});
