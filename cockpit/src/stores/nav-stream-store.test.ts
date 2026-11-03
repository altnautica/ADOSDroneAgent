import { beforeEach, describe, expect, it } from "vitest";

import { parseFollowState } from "@/shared/hud/nav-types";
import { ageOf, reduceNavFrame, useNavStreamStore } from "@/stores/nav-stream-store";

const IDENT = [1, 0, 0, 0, 1, 0, 0, 0, 1];
const pose = (x: number, y: number) => ({
  kind: "pose" as const,
  data: { R: IDENT, t: [x, y, 0] as [number, number, number], health: "good" as const, frame: "map_frd", source: null },
});
const designated = { cameraId: "main", trackId: 12, bbox: { x: 0, y: 0, width: 10, height: 20 } };

describe("reduceNavFrame", () => {
  it("stamps each kind with its arrival time", () => {
    const follow = parseFollowState({ phase: "following" })!;
    expect(reduceNavFrame({ trail: [], designated: null }, { kind: "follow", data: follow }, 42)).toEqual({
      follow: { data: follow, at: 42 },
    });
  });

  it("extends the SLAM trail with each pose", () => {
    const a = reduceNavFrame({ trail: [], designated: null }, pose(0, 0), 1000);
    expect(a.trail).toEqual([{ x: 0, y: 0, at: 1000, health: "good" }]);
    const b = reduceNavFrame({ trail: a.trail!, designated: null }, pose(1, 0), 2000);
    expect(b.trail).toHaveLength(2);
    expect(b.pose?.at).toBe(2000);
  });

  it("moves the designation to a re-acquired track on the same camera", () => {
    const ev = { topic: "follow.reacquired" as const, trackId: 15, similarity: 0.86, gapS: 1.8 };
    const r = reduceNavFrame({ trail: [], designated }, { kind: "follow_event", data: ev }, 5);
    expect(r.designated).toEqual({ ...designated, trackId: 15 });
    expect(r.event).toEqual({ data: ev, at: 5 });
    expect(reduceNavFrame({ trail: [], designated: null }, { kind: "follow_event", data: ev }, 5).designated).toBeNull();
  });

  it("clears the designation on follow.lost", () => {
    const ev = { topic: "follow.lost" as const, trackId: 12, reason: "timeout" };
    expect(reduceNavFrame({ trail: [], designated }, { kind: "follow_event", data: ev }, 5).designated).toBeNull();
  });
});

describe("useNavStreamStore", () => {
  beforeEach(() => useNavStreamStore.getState().clear());

  it("ingests frames and clears back to empty", () => {
    const s = useNavStreamStore.getState();
    s.designate(designated);
    s.setCommandedSet(2);
    s.ingest(pose(1, 2), 100);
    expect(useNavStreamStore.getState().pose?.at).toBe(100);
    expect(useNavStreamStore.getState().lastCommandedSet).toBe(2);
    s.clear();
    const c = useNavStreamStore.getState();
    expect(c.pose).toBeNull();
    expect(c.designated).toBeNull();
    expect(c.trail).toEqual([]);
    expect(c.link).toBe("idle");
  });

  it("merges occupancy and stamps the poll", () => {
    useNavStreamStore.getState().mergeOccupancy({ seq: 7, frame: "map_frd", resM: 0.05, tileCells: 64, tiles: [] }, 900);
    const s = useNavStreamStore.getState();
    expect(s.occupancy.seq).toBe(7);
    expect(s.occupancyAt).toBe(900);
  });

  it("ages a stamped frame, Infinity when absent", () => {
    expect(ageOf({ data: 1, at: 100 }, 350)).toBe(250);
    expect(ageOf(null, 350)).toBe(Infinity);
  });
});
