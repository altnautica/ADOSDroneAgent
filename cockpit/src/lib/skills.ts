// The built-in flight skills the on-box cockpit can actually drive, plus the
// gating that keeps the bar honest.
//
// The ONLY control path from this cockpit to the flight controller is the agent's
// `POST /api/command`, which sends a fixed set of high-level MAVLink COMMAND_LONG
// actions — arm, disarm, takeoff, land, rtl, and set-mode. There is no
// virtual-stick / MANUAL_CONTROL and no guided goto over this REST surface (those
// live on the plugin host's flight facade, not here), so this catalog contains
// ONLY commands the agent can genuinely execute. A skill the node cannot drive is
// never rendered; a skill that is momentarily inapplicable (already armed, no FC
// link) renders disabled with a plain reason.

/** `HEARTBEAT.autopilot` value for PX4 (`MAV_AUTOPILOT_PX4`). Selects the PX4
 *  flight-mode names; any other value (or none) uses the ArduPilot names. Mirrors
 *  the agent command route's family switch. */
export const AUTOPILOT_PX4 = 12;

/** A skill's category, so the bar can group the core flight actions apart from
 *  the flight-mode presets. */
export type SkillCategory = "flight" | "mode";

/** One built-in skill: a labelled action that maps to a `POST /api/command`
 *  `{ cmd, args }`. `confirm` marks the high-consequence actions the bar guards
 *  with a confirmation step. */
export interface Skill {
  /** Stable id (unique across the catalog); e.g. `arm`, `mode:LOITER`. */
  id: string;
  label: string;
  /** The `/api/command` command name. */
  cmd: "arm" | "disarm" | "takeoff" | "land" | "rtl" | "mode";
  /** Command args (e.g. the mode name for a `mode` preset). */
  args: (string | number)[];
  /** Whether the bar guards this action behind an explicit confirm. */
  confirm: boolean;
  category: SkillCategory;
}

/** The core flight actions, always present when an FC link is live. Arm and
 *  disarm are both listed; the bar shows whichever one applies to the current
 *  armed state, and the gating below disables the inapplicable one. */
export const CORE_SKILLS: Skill[] = [
  { id: "arm", label: "Arm", cmd: "arm", args: [], confirm: true, category: "flight" },
  { id: "disarm", label: "Disarm", cmd: "disarm", args: [], confirm: true, category: "flight" },
  { id: "takeoff", label: "Takeoff", cmd: "takeoff", args: [], confirm: true, category: "flight" },
  { id: "land", label: "Land", cmd: "land", args: [], confirm: true, category: "flight" },
  { id: "rtl", label: "RTL", cmd: "rtl", args: [], confirm: true, category: "flight" },
];

/** ArduPilot mode presets (names resolved by the agent's copter mode table). */
const ARDUPILOT_MODE_PRESETS: ReadonlyArray<{ name: string; label: string }> = [
  { name: "STABILIZE", label: "Stabilize" },
  { name: "ALT_HOLD", label: "Alt Hold" },
  { name: "LOITER", label: "Loiter" },
  { name: "GUIDED", label: "Guided" },
];

/** PX4 mode presets (names resolved by the agent's PX4 mode table). */
const PX4_MODE_PRESETS: ReadonlyArray<{ name: string; label: string }> = [
  { name: "ALTITUDE", label: "Altitude" },
  { name: "POSITION", label: "Position" },
  { name: "LOITER", label: "Hold" },
  { name: "MISSION", label: "Mission" },
];

/**
 * The mode-preset skills for the FC's autopilot family. Only names valid for that
 * family are offered, so the bar never renders a mode the agent would reject — an
 * unknown `autopilot` (no heartbeat yet) defaults to the ArduPilot set, the
 * primary target. Mode changes are lower-consequence than arming/landing, so they
 * are not confirm-guarded.
 */
export function modePresetsFor(autopilot: number | null | undefined): Skill[] {
  const presets = autopilot === AUTOPILOT_PX4 ? PX4_MODE_PRESETS : ARDUPILOT_MODE_PRESETS;
  return presets.map((p) => ({
    id: `mode:${p.name}`,
    label: p.label,
    cmd: "mode" as const,
    args: [p.name],
    confirm: false,
    category: "mode" as const,
  }));
}

/** Inputs that decide whether a skill is currently drivable. */
export interface SkillContext {
  /** Whether a live MAVLink flight-controller link is present (fresh telemetry).
   *  The COMMAND_LONG path needs this; an MSP FC (Betaflight/iNav) has no MAVLink
   *  link, so these commands correctly gate off. */
  fcConnected: boolean;
  /** Whether the vehicle is armed (from the live telemetry snapshot). */
  armed: boolean;
}

/** The resolved drivability of a skill: enabled, or disabled with a plain
 *  operator-facing reason. */
export interface SkillState {
  enabled: boolean;
  reason?: string;
}

/**
 * Whether a skill can be driven right now, and if not, why. Without a live FC
 * link nothing is drivable (the command would never reach a flight controller).
 * With a link, arm is inapplicable while armed and disarm while disarmed; every
 * other action is available. The reason strings are what the bar shows on a
 * disabled control (Rule 44 — never a control the node cannot drive without a
 * plain reason).
 */
export function resolveSkillState(skill: Skill, ctx: SkillContext): SkillState {
  if (!ctx.fcConnected) {
    return { enabled: false, reason: "No flight controller link" };
  }
  if (skill.cmd === "arm" && ctx.armed) {
    return { enabled: false, reason: "Already armed" };
  }
  if (skill.cmd === "disarm" && !ctx.armed) {
    return { enabled: false, reason: "Not armed" };
  }
  return { enabled: true };
}
