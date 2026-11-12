// Every flight-controller mode the Mode sheet offers, per firmware family,
// with its confirm tier. Names are the ones `POST /api/command` `mode`
// accepts (the agent's mode table: `ados-protocol` flight_modes). The
// recovery modes (`FC_RECOVERY_MODES`, the one recovery set) need an 800 ms
// hold; every other mode a 1500 ms hold.

import { FC_RECOVERY_MODES } from "@/lib/fc-failsafe";

export type FcFamily = "copter" | "plane" | "px4";

export interface FcMode {
  name: string;
  label: string;
  holdMs: number;
}

const RECOVERY_HOLD_MS = 800;
const MODE_HOLD_MS = 1500;

function modes(all: readonly [string, string][]): FcMode[] {
  return all.map(([name, label]) => ({
    name,
    label,
    holdMs: FC_RECOVERY_MODES[name] ? RECOVERY_HOLD_MS : MODE_HOLD_MS,
  }));
}

const COPTER = modes(
  [
    ["LOITER", "Loiter"],
    ["POSHOLD", "PosHold"],
    ["ALT_HOLD", "Alt Hold"],
    ["BRAKE", "Brake"],
    ["GUIDED", "Guided"],
    ["AUTO", "Auto"],
    ["RTL", "RTL"],
    ["SMART_RTL", "Smart RTL"],
    ["LAND", "Land"],
    ["CIRCLE", "Circle"],
    ["STABILIZE", "Stabilize"],
    ["ACRO", "Acro"],
    ["SPORT", "Sport"],
    ["DRIFT", "Drift"],
    ["FLOWHOLD", "FlowHold"],
    ["ZIGZAG", "ZigZag"],
    ["AUTOTUNE", "AutoTune"],
  ],
);

const PLANE = modes(
  [
    ["LOITER", "Loiter"],
    ["CRUISE", "Cruise"],
    ["FBWA", "FBWA"],
    ["FBWB", "FBWB"],
    ["GUIDED", "Guided"],
    ["AUTO", "Auto"],
    ["RTL", "RTL"],
    ["CIRCLE", "Circle"],
    ["MANUAL", "Manual"],
    ["STABILIZE", "Stabilize"],
    ["ACRO", "Acro"],
    ["TRAINING", "Training"],
    ["THERMAL", "Thermal"],
    ["QHOVER", "QHover"],
    ["QLOITER", "QLoiter"],
    ["QRTL", "QRTL"],
    ["QLAND", "QLand"],
    ["QSTABILIZE", "QStabilize"],
    ["QACRO", "QAcro"],
  ],
);

const PX4 = modes(
  [
    ["POSITION", "Position"],
    ["ALTITUDE", "Altitude"],
    ["LOITER", "Hold"],
    ["MISSION", "Mission"],
    ["RTL", "Return"],
    ["LAND", "Land"],
    ["STABILIZED", "Stabilized"],
    ["ACRO", "Acro"],
    ["MANUAL", "Manual"],
  ],
);

/** The family from HEARTBEAT `autopilot` (12 = PX4) and the pilot's vehicle
 *  class (fixed-wing and VTOL fly ArduPlane). */
export function fcFamily(autopilot: number | null | undefined, vehicleClass: string | null | undefined): FcFamily {
  if (autopilot === 12) return "px4";
  return vehicleClass === "fixedwing" || vehicleClass === "vtol" ? "plane" : "copter";
}

export function fcModesFor(family: FcFamily): FcMode[] {
  return family === "px4" ? PX4 : family === "plane" ? PLANE : COPTER;
}
