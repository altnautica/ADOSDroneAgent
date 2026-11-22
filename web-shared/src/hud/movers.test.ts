import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { MOVER_KIND_COLOR, moverKindLabel, moverMarks, moversLine, nearestPersonM } from "./movers";
import { parseMovers, type Mover } from "./pilot-fields";

describe("tracked movers", () => {
  it("parses every well-formed mover and reads an unknown kind as unknown", () => {
    const m = parseMovers([
      { id: 3, kind: "person", source: "vision", ne: [12.5, -1.2], vel_ne: [0, 1.4], sigma_m: 0.3, age_ms: 100 },
      { id: 4, kind: "drone", source: "scan", ne: [1, 2], vel_ne: [0.5, 0], sigma_m: 0.2, age_ms: 0 },
      { id: 5, kind: "vehicle", ne: [1], vel_ne: [0, 0] },
      "junk",
    ]);
    expect(m).toEqual([
      { id: 3, kind: "person", source: "vision", ne: [12.5, -1.2], velNe: [0, 1.4], sigmaM: 0.3, ageMs: 100 },
      { id: 4, kind: "unknown", source: "scan", ne: [1, 2], velNe: [0.5, 0], sigmaM: 0.2, ageMs: 0 },
    ]);
  });

  it("is empty when the pilot sends none, and rides on the pilot state", () => {
    expect(parseMovers(undefined)).toEqual([]);
    const base = { owner: "pilot", fc_mode: 4, armed: true };
    expect(parsePilotState(base)?.movers).toEqual([]);
    const withOne = {
      ...base,
      movers: [{ id: 1, kind: "vehicle", source: "vision", ne: [5, 5], vel_ne: [3, 0], sigma_m: 0.3, age_ms: 0 }],
    };
    expect(parsePilotState(withOne)?.movers[0]?.kind).toBe("vehicle");
  });
});

describe("movers on the map and in the status line", () => {
  const person = (id: number, ne: [number, number]): Mover => ({
    id,
    kind: "person",
    source: "vision",
    ne,
    velNe: [0, 0],
    sigmaM: 0.3,
    ageMs: 0,
  });
  const car: Mover = { id: 9, kind: "vehicle", source: "vision", ne: [1, 1], velNe: [4, 0], sigmaM: 0.5, ageMs: 1200 };

  it("counts movers and names the nearest person's horizontal distance", () => {
    expect(moversLine([], { north: 0, east: 0 })).toBeNull();
    const all = [person(1, [6, 8]), person(2, [-3, 4]), car];
    expect(nearestPersonM(all, { north: 0, east: 0 })).toBe(5);
    expect(moversLine(all, { north: 0, east: 0 })).toBe("Movers: 3 · nearest person 5.0 m");
    // No drone position or no person: the count alone.
    expect(moversLine(all, null)).toBe("Movers: 3");
    expect(moversLine([car], { north: 0, east: 0 })).toBe("Movers: 1");
  });

  it("leads a moving mover by 2 s, draws a still one without an arrow, and fades one unseen over 1 s", () => {
    expect(moverMarks([car, person(1, [2, 2])])).toEqual([
      { id: 9, kind: "vehicle", ne: [1, 1], leadNe: [9, 1], sigmaM: 0.5, faded: true },
      { id: 1, kind: "person", ne: [2, 2], leadNe: null, sigmaM: 0.3, faded: false },
    ]);
  });

  it("words and colours every kind", () => {
    expect(moverKindLabel("person")).toBe("Person");
    expect(moverKindLabel("vehicle")).toBe("Vehicle");
    expect(moverKindLabel("unknown")).toBe("Moving object");
    expect(new Set(Object.values(MOVER_KIND_COLOR)).size).toBe(3);
  });
});
