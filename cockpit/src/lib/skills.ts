// The skills the on-box cockpit can drive, their confirm gestures, and the
// gating that keeps the Skill Bar honest.
//
// Built-in skills map onto the agent's `POST /api/command` (arm, disarm,
// takeoff, land, rtl, mode, killSwitch, pauseMission, resumeMission). Extension
// skills flip a plugin config key (`PUT /api/plugins/{id}/config`). A skill
// the node cannot drive right now renders disabled with a plain reason.
//
// Confirm gestures:
//   tap     — fires immediately (pause, hold-type recovery modes)
//   hold    — 800 ms press-and-hold on the confirm sheet (or the bound
//             gamepad button / Enter held); takeoff carries an altitude
//   slide   — slide-to-confirm on touch; a 1500 ms hold on gamepad/keyboard
//   guarded — kill: the first activation arms a 3 s guard, a 1500 ms hold
//             inside that window fires

import type { FlightCommand } from "@/lib/api";

export type ConfirmGesture = "tap" | "hold" | "slide" | "guarded";
export type SkillCategory = "flight" | "mode" | "safety" | "extension";
export type ArmRequirement = "any" | "armed" | "disarmed";

export const HOLD_MS = 800;
export const SLIDE_HOLD_MS = 1500;
export const GUARD_HOLD_MS = 1500;
export const GUARD_WINDOW_MS = 3000;
export const DEFAULT_TAKEOFF_ALT_M = 10;

/** `HEARTBEAT.autopilot` value for PX4 (`MAV_AUTOPILOT_PX4`). */
export const AUTOPILOT_PX4 = 12;

export interface ExtensionBinding {
  pluginId: string;
  configKey: string;
  toggle: boolean;
  stateTopic: string | null;
}

export interface Skill {
  /** Stable id: `arm`, `mode:LOITER`, or `<pluginId>:<localId>`. */
  id: string;
  label: string;
  category: SkillCategory;
  gesture: ConfirmGesture;
  armRequirement: ArmRequirement;
  /** Built-in: the `/api/command` call. */
  command?: { cmd: FlightCommand; args: (string | number)[] };
  /** Extension: the config key the skill flips. */
  extension?: ExtensionBinding;
  /** Takeoff: the confirm sheet carries an altitude stepper. */
  takesAltitude?: boolean;
  /** Lucide icon name (extension skills declare one). */
  icon?: string;
}

/** The hold the gesture needs, in ms (0 for tap). */
export function holdMsFor(gesture: ConfirmGesture): number {
  switch (gesture) {
    case "tap":
      return 0;
    case "hold":
      return HOLD_MS;
    case "slide":
      return SLIDE_HOLD_MS;
    case "guarded":
      return GUARD_HOLD_MS;
  }
}

function builtin(
  id: string,
  label: string,
  cmd: FlightCommand,
  gesture: ConfirmGesture,
  armRequirement: ArmRequirement,
  category: SkillCategory = "flight",
): Skill {
  return { id, label, category, gesture, armRequirement, command: { cmd, args: [] } };
}

export const CORE_SKILLS: Skill[] = [
  builtin("arm", "Arm", "arm", "slide", "disarmed"),
  builtin("disarm", "Disarm", "disarm", "hold", "armed"),
  { ...builtin("takeoff", "Takeoff", "takeoff", "hold", "armed"), takesAltitude: true },
  builtin("land", "Land", "land", "hold", "armed"),
  builtin("rtl", "RTL", "rtl", "hold", "armed"),
  builtin("pause", "Pause", "pausemission", "tap", "armed"),
  builtin("resume", "Resume", "resumemission", "hold", "armed"),
  builtin("kill", "Kill", "killswitch", "guarded", "armed", "safety"),
];

export const CORE_BY_ID: Record<string, Skill> = Object.fromEntries(
  CORE_SKILLS.map((s) => [s.id, s]),
);

interface ModePreset {
  name: string;
  label: string;
  /** Hold-type recovery modes fire on a tap; every other mode needs a hold. */
  recovery: boolean;
}

const ARDUPILOT_MODE_PRESETS: readonly ModePreset[] = [
  { name: "STABILIZE", label: "Stabilize", recovery: false },
  { name: "ALT_HOLD", label: "Alt Hold", recovery: true },
  { name: "LOITER", label: "Loiter", recovery: true },
  { name: "BRAKE", label: "Brake", recovery: true },
  { name: "GUIDED", label: "Guided", recovery: false },
];

const PX4_MODE_PRESETS: readonly ModePreset[] = [
  { name: "ALTITUDE", label: "Altitude", recovery: true },
  { name: "POSITION", label: "Position", recovery: true },
  { name: "LOITER", label: "Hold", recovery: true },
  { name: "MISSION", label: "Mission", recovery: false },
];

/** The mode presets valid for the FC's autopilot family (ArduPilot when the
 *  family is not known yet). */
export function modePresetsFor(autopilot: number | null | undefined): Skill[] {
  const presets = autopilot === AUTOPILOT_PX4 ? PX4_MODE_PRESETS : ARDUPILOT_MODE_PRESETS;
  return presets.map((p) => ({
    id: `mode:${p.name}`,
    label: p.label,
    category: "mode",
    gesture: p.recovery ? "tap" : "hold",
    armRequirement: "any",
    command: { cmd: "mode", args: [p.name] },
  }));
}

/** An extension skill's state as the plugin reports it. */
export interface ReportedSkillState {
  state: "active" | "idle" | "disabled";
  badge?: string;
  reason?: string;
}

/** Inputs that decide whether a skill is drivable. */
export interface SkillContext {
  /** A flight controller link this node can command through: a drone's own
   *  MAVLink FC, or a ground station's fresh relayed aircraft. */
  fcConnected: boolean;
  /** Fresh vehicle telemetry is arriving (attitude moving). */
  live: boolean;
  armed: boolean;
  /** Extension skills: the plugin's last reported state, by skill id. */
  reported?: Record<string, ReportedSkillState | undefined>;
}

export interface SkillState {
  enabled: boolean;
  reason?: string;
  active?: boolean;
  badge?: string;
}

/**
 * Whether a skill can be driven right now, and if not, why. Built-in commands
 * need a commandable FC link with fresh telemetry: a link that answers but
 * carries no live vehicle state is not one to fly through. Extension skills
 * act on the plugin, so they are gated by the plugin's own reported state;
 * any arm requirement still needs a live vehicle to read the arm state from.
 */
export function resolveSkillState(skill: Skill, ctx: SkillContext): SkillState {
  const vehicleKnown = ctx.fcConnected && ctx.live;
  if (skill.command && !vehicleKnown) {
    return { enabled: false, reason: "No flight controller link" };
  }
  if (skill.armRequirement !== "any") {
    if (!vehicleKnown) return { enabled: false, reason: "No flight controller link" };
    if (skill.armRequirement === "armed" && !ctx.armed) return { enabled: false, reason: "Not armed" };
    if (skill.armRequirement === "disarmed" && ctx.armed) {
      return { enabled: false, reason: "Already armed" };
    }
  }
  if (skill.extension) {
    const reported = ctx.reported?.[skill.id];
    if (reported?.state === "disabled") {
      return { enabled: false, reason: reported.reason ?? "Unavailable" };
    }
    return { enabled: true, active: reported?.state === "active", badge: reported?.badge };
  }
  return { enabled: true };
}
