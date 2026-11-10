// The live navigation feed this node's Feed reads: the latest frame of each
// nav-stream kind (follow state, vision-nav state, SLAM pose, planar scan,
// map status) with the epoch-ms time it arrived, the latest discrete follow
// event, the live occupancy grid with its poll time, the SLAM trail, and the
// operator's designated target. Written by `lib/nav-stream` (or the demo
// mock); every reader checks the age before drawing, so a dead link never
// paints a live-looking target, radar or chip.

import { create } from "zustand";

import {
  EMPTY_OCCUPANCY,
  appendTrail,
  mergeOccupancy,
  type OccupancyGrid,
  type TrailPoint,
} from "@/shared/hud/local-frame";
import type {
  FollowEvent,
  FollowState,
  MapStatus,
  NavPose,
  NavScan,
  NavState,
  OccupancyLive,
} from "@/shared/hud/nav-types";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import type { DesignatedRef } from "@/shared/hud/target-state";

/** One received frame and the epoch ms it arrived. */
export interface Stamped<T> {
  data: T;
  at: number;
}

/** A parsed nav-stream frame. */
export type NavFrame =
  | { kind: "pose"; data: NavPose }
  | { kind: "scan"; data: NavScan }
  | { kind: "nav"; data: NavState }
  | { kind: "follow"; data: FollowState }
  | { kind: "map"; data: MapStatus }
  | { kind: "follow_event"; data: FollowEvent }
  | { kind: "pilot"; data: PilotState };

export type NavLinkState = "idle" | "connected" | "reconnecting" | "closed";

export interface NavStreamState {
  follow: Stamped<FollowState> | null;
  nav: Stamped<NavState> | null;
  pose: Stamped<NavPose> | null;
  scan: Stamped<NavScan> | null;
  map: Stamped<MapStatus> | null;
  event: Stamped<FollowEvent> | null;
  /** The command authority's state (owner, control mode, mission, …). */
  pilot: Stamped<PilotState> | null;
  occupancy: OccupancyGrid;
  /** Epoch ms of the last successful occupancy poll, or null. */
  occupancyAt: number | null;
  trail: TrailPoint[];
  /** The target this cockpit designated (or follow-me re-acquired). */
  designated: DesignatedRef | null;
  /** The EKF source set this cockpit last commanded, or null. */
  lastCommandedSet: 1 | 2 | 3 | null;
  /** `EK3_SRC{1,2,3}_{POSXY,VELXY,YAW}` as the FC reported them, or null
   *  before they were read. A parameter the FC did not report is absent. */
  ekfParams: Record<string, number> | null;
  link: NavLinkState;
  /** The stream is down and the feed comes from the 2 Hz status poll (a radio
   *  relay): the values update at a reduced rate. */
  radioFallback: boolean;

  ingest: (frame: NavFrame, at?: number) => void;
  mergeOccupancy: (live: OccupancyLive, at?: number) => void;
  designate: (ref: DesignatedRef | null) => void;
  setCommandedSet: (set: 1 | 2 | 3) => void;
  setLink: (link: NavLinkState) => void;
  setRadioFallback: (radioFallback: boolean) => void;
  setEkfParams: (params: Record<string, number>) => void;
  clear: () => void;
}

const EMPTY = {
  follow: null,
  nav: null,
  pose: null,
  scan: null,
  map: null,
  event: null,
  pilot: null,
  occupancy: EMPTY_OCCUPANCY,
  occupancyAt: null,
  trail: [] as TrailPoint[],
  designated: null,
  lastCommandedSet: null,
  ekfParams: null as Record<string, number> | null,
  link: "idle" as NavLinkState,
  radioFallback: false,
};

/**
 * The store change one frame makes. Pure, so the reducer rules are testable:
 * a pose extends the SLAM trail; a `follow.reacquired` moves the designation
 * to the re-acquired track on the same camera (follow-me already designated
 * it on the agent); a `follow.lost` clears the designation.
 */
export function reduceNavFrame(
  s: Pick<NavStreamState, "trail" | "designated">,
  frame: NavFrame,
  at: number,
): Partial<NavStreamState> {
  switch (frame.kind) {
    case "pose":
      return {
        pose: { data: frame.data, at },
        trail: appendTrail(s.trail, { x: frame.data.t[0], y: frame.data.t[1], at, health: frame.data.health }),
      };
    case "scan":
      return { scan: { data: frame.data, at } };
    case "nav":
      return { nav: { data: frame.data, at } };
    case "follow":
      return { follow: { data: frame.data, at } };
    case "map":
      return { map: { data: frame.data, at } };
    case "pilot":
      return { pilot: { data: frame.data, at } };
    case "follow_event": {
      const ev = frame.data;
      if (ev.topic === "follow.reacquired") {
        const designated = s.designated ? { ...s.designated, trackId: ev.trackId } : s.designated;
        return { event: { data: ev, at }, designated };
      }
      return { event: { data: ev, at }, designated: null };
    }
  }
}

export const useNavStreamStore = create<NavStreamState>((set) => ({
  ...EMPTY,
  ingest: (frame, at = Date.now()) => set((s) => reduceNavFrame(s, frame, at)),
  mergeOccupancy: (live, at = Date.now()) =>
    set((s) => ({ occupancy: mergeOccupancy(s.occupancy, live), occupancyAt: at })),
  designate: (designated) => set({ designated }),
  setCommandedSet: (lastCommandedSet) => set({ lastCommandedSet }),
  setLink: (link) => set({ link }),
  setRadioFallback: (radioFallback) => set((s) => (s.radioFallback === radioFallback ? s : { radioFallback })),
  setEkfParams: (ekfParams) => set({ ekfParams }),
  clear: () => set({ ...EMPTY }),
}));

/** The age of a stamped frame at `now`, or Infinity when absent. */
export function ageOf(s: Stamped<unknown> | null, now: number): number {
  return s ? now - s.at : Infinity;
}
