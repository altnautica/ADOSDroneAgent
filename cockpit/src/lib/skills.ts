// The skills the on-box cockpit can drive and the gate that keeps every
// surface of them (the left column, the skill picker, keyboard chords and
// controller bindings) honest.
//
// Built-in skills are the cockpit spec's table (`builtins.json`, through
// `@/lib/cockpit-spec`): arm, disarm, takeoff, land, rth, pause, resume,
// take_control and kill, each with its arm requirement, confirm tier and
// stale-link rule. They go to the drone's pilot as `action`s while a pilot
// state is present, and to `POST /api/command` otherwise (take_control has no
// command twin and is unavailable without the pilot). Extension skills flip a
// plugin config key (`PUT /api/plugins/{id}/config`) and read their state back
// from the plugin's state topic. A skill the node cannot drive right now
// resolves disabled with a plain reason.

import type { FlightCommand } from "@/lib/api";
import { BUILTINS, type BuiltinConfirm } from "@/lib/cockpit-spec";
import type { VehicleClass } from "@/shared/hud/cockpit-frames";
import { refusalText } from "@/shared/hud/refusal";

export type SkillCategory = "flight" | "behavior" | "camera" | "navigation" | "utility";
export type PluginSkillCategory = Exclude<SkillCategory, "flight">;
export type ArmRequirement = "any" | "armed" | "disarmed" | "armed_landed" | "disarmed_or_landed";
export type SkillConfirm = BuiltinConfirm;

/** Press-and-hold timings of the cockpit's generic confirm controls (the hold,
 *  slide and guarded buttons used across the settings screens). */
export const HOLD_MS = 800;
export const SLIDE_HOLD_MS = 1500;
export const GUARD_HOLD_MS = 1500;
export const GUARD_WINDOW_MS = 3000;

/** A plugin skill whose manifest sets `confirm: true` needs this hold. */
export const PLUGIN_CONFIRM_MS = 800;
/** Takeoff altitude offered when the command path (no pilot) takes off. */
export const DEFAULT_TAKEOFF_ALT_M = 10;
/** A pilot state older than this is a stale link (the chip's LINK rule). */
export const PILOT_FRESH_MS = 2000;

export const VEHICLE_CLASSES: readonly VehicleClass[] = ["multirotor", "fixedwing", "vtol"];

/** How a built-in reaches the drone. */
export interface BuiltinBinding {
  /** The pilot `action` name. */
  pilotAction: string;
  /** The `POST /api/command` twin when no pilot runs; null = pilot only. */
  command: FlightCommand | null;
  /** On a stale link: disabled with a reason, or sent blind. */
  staleLink: "disable" | "send_blind";
  /** Keyboard chord from the spec (`shift+a`, `space`), or null. */
  keyboard: string | null;
  armsFirst: boolean;
  /** Pause, land, rth and kill stop every running motion skill. */
  stopsMotion: boolean;
}

/** A plugin parameter shown in a skill's Tweak panel, from the plugin's
 *  `gcs.contributes.parameters` schema. */
export type SkillParam =
  | ({ kind: "number"; min: number; max: number; step: number; defaultValue: number | null } & ParamBase)
  | ({ kind: "enum"; values: string[]; defaultValue: string | null } & ParamBase)
  | ({ kind: "boolean"; defaultValue: boolean | null } & ParamBase);

interface ParamBase {
  key: string;
  label: string;
  /** Shown only while another parameter has this value. */
  visibleIf: { key: string; equals: ParamValue } | null;
}

export type ParamValue = string | number | boolean;

/** A two-value enum parameter offered as a segmented quick setting on a toggle
 *  skill (follow-me's `reacquire_mode`, STRICT | SMART). */
export interface SkillSetting {
  key: string;
  values: [string, string];
  defaultValue: string | null;
}

export interface ExtensionBinding {
  pluginId: string;
  /** The plugin's display name, the picker's group heading. */
  pluginName: string;
  /** The skill's id inside the plugin manifest. */
  localId: string;
  configKey: string;
  toggle: boolean;
  stateTopic: string;
  /** Commands the vehicle: needs the pilot lease, preempted by built-ins. */
  motion: boolean;
  nudge: boolean;
  oneShot: boolean;
  vehicleClasses: readonly VehicleClass[];
  defaultBinding: { key: string | null; gamepadButton: number | string | null } | null;
}

export interface Skill {
  /** Built-in id (`arm`, `rth`) or `<pluginId>:<skillId>`. */
  id: string;
  label: string;
  category: SkillCategory;
  confirm: SkillConfirm;
  armRequirement: ArmRequirement;
  /** Icon name from the spec or the manifest. */
  icon?: string;
  builtin?: BuiltinBinding;
  extension?: ExtensionBinding;
  /** Takeoff over the command path carries an altitude stepper. */
  takesAltitude?: boolean;
  /** Extension toggle skills: the plugin's two-value quick settings. */
  settings?: SkillSetting[];
  /** Extension skills: the Tweak panel's parameters, in manifest order. */
  params?: SkillParam[];
}

const COMMAND_TWIN: Readonly<Record<string, FlightCommand | null>> = {
  arm: "arm",
  disarm: "disarm",
  takeoff: "takeoff",
  land: "land",
  rth: "rtl",
  pause: "pausemission",
  resume: "resumemission",
  kill: "killswitch",
  take_control: null,
};

const STOPS_MOTION: Readonly<Record<string, true>> = { pause: true, land: true, rth: true, kill: true };

export const BUILTIN_SKILLS: readonly Skill[] = BUILTINS.map((b) => ({
  id: b.id,
  label: b.label,
  category: "flight",
  icon: b.icon,
  confirm: b.confirm,
  armRequirement: b.armRequirement,
  takesAltitude: b.id === "takeoff" ? true : undefined,
  builtin: {
    pilotAction: b.pilotAction,
    command: COMMAND_TWIN[b.id] ?? null,
    staleLink: b.staleLink,
    keyboard: b.keyboard,
    armsFirst: b.armsFirst,
    stopsMotion: STOPS_MOTION[b.id] === true,
  },
}));

export const BUILTIN_SKILL_BY_ID: Readonly<Record<string, Skill>> = Object.fromEntries(
  BUILTIN_SKILLS.map((s) => [s.id, s]),
);

/** The cockpit action id that fires a skill: the built-in id, or
 *  `skill:<pluginId>:<skillId>` for a plugin skill. */
export function skillActionId(skill: Skill): string {
  return skill.extension ? `skill:${skill.extension.pluginId}:${skill.extension.localId}` : skill.id;
}

/** An extension skill's state as the plugin reports it. */
export interface ReportedSkillState {
  state: "active" | "idle" | "disabled";
  badge?: string;
  reason?: string;
  /** The applied value of each quick setting and parameter, as echoed. */
  echo?: Record<string, ParamValue>;
  /** Epoch ms this cockpit last received the skill's state topic. */
  at?: number;
}

/** An activation write awaiting the plugin's echo. */
export interface PendingWrite {
  want: boolean;
  /** Epoch ms of the write. */
  at: number;
}

/** What the gate needs from the drone's pilot state (the `pilot` nav kind). */
export interface PilotView {
  armed: boolean;
  landed: boolean;
  vehicleClass: VehicleClass | null;
  /** The pilot state is no older than `PILOT_FRESH_MS`. */
  fresh: boolean;
}

/** Inputs that decide whether a skill is drivable. */
export interface SkillContext {
  /** A flight controller link this node can command through (`fc_connected`).
   *  A relayed aircraft heard off the radio is not one. */
  fcConnected: boolean;
  /** Fresh vehicle telemetry is arriving. */
  live: boolean;
  armed: boolean;
  /** The arm state at the last live sample, for blind sends on a stale link. */
  lastArmed?: boolean | null;
  /** Why commands are unavailable, when more specific than "No flight
   *  controller link". */
  linkReason?: string;
  /** The drone's pilot state; null when no pilot state was ever received
   *  (the drone does not run the pilot). */
  pilot?: PilotView | null;
  /** The pilot link socket (`/api/v1/pilot/stream`) is open. */
  pilotLinkOpen?: boolean;
  /** Extension skills: the plugin's last reported state, by skill id. */
  reported?: Record<string, ReportedSkillState | undefined>;
  /** Extension toggle writes awaiting their echo, by skill id. */
  pending?: Record<string, PendingWrite | undefined>;
  /** Epoch ms now (state freshness); defaults to `Date.now()`. */
  now?: number;
}

export interface SkillState {
  enabled: boolean;
  reason?: string;
  active?: boolean;
  badge?: string;
  /** A write awaits its echo: draw the pending ring. */
  pending?: boolean;
  /** The plugin's state is older than the freshness window: dim the slot. */
  stale?: boolean;
  /** Whole seconds since the plugin's state was last seen, while stale. */
  lastSeenS?: number;
}

/** Plugin state older than this dims the skill ("last seen n s"). */
export const STATE_FRESH_MS = 3000;

/** How a built-in would be sent right now: to the pilot, over
 *  `POST /api/command`, or not at all. */
export function builtinRoute(skill: Skill, ctx: SkillContext): "pilot" | "command" | null {
  const b = skill.builtin;
  if (!b) return null;
  const commandOk = b.command !== null && ctx.fcConnected;
  // A stalled pilot (state older than its freshness window) loses the route
  // to `POST /api/command` while the FC link is live: RTH, Land and Kill must
  // not depend on a pilot that stopped answering.
  if (ctx.pilot && ctx.pilotLinkOpen && (ctx.pilot.fresh || !commandOk)) return "pilot";
  if (commandOk) return "command";
  return null;
}

function disabled(reason: string): SkillState {
  return { enabled: false, reason };
}

/** The arm requirement against an arm state and a landed state (null =
 *  unknown). Returns the reason it fails, or null when it holds. */
export function armGateReason(req: ArmRequirement, armed: boolean, landed: boolean | null): string | null {
  switch (req) {
    case "any":
      return null;
    case "armed":
      return armed ? null : refusalText("not_armed");
    case "disarmed":
      return armed ? refusalText("armed") : null;
    case "armed_landed":
      if (!armed) return refusalText("not_armed");
      if (landed === null) return "Landed state unknown";
      return landed ? null : refusalText("not_landed");
    case "disarmed_or_landed":
      if (!armed) return null;
      if (landed === null) return "Landed state unknown";
      return landed ? null : refusalText("not_landed");
  }
}

function resolveBuiltin(skill: Skill, b: BuiltinBinding, ctx: SkillContext): SkillState {
  // The emergency stop is always offered: its 3 s hold and confirm guard it,
  // and a send with no route says so when it fails.
  if (skill.id === "kill") return { enabled: true };
  const pilot = ctx.pilot ?? null;
  const noLink = ctx.linkReason ?? refusalText("no_fc");
  const route = builtinRoute(skill, ctx);
  if (route === null) {
    if (!pilot && b.command === null) return disabled(refusalText("pilot_unavailable"));
    if (b.command === null) return disabled(refusalText("no_pilot_link"));
    return disabled(noLink);
  }
  const fresh = route === "pilot" ? pilot!.fresh : ctx.live;
  if (!fresh && b.staleLink === "disable") return disabled(route === "pilot" ? refusalText("no_pilot_link") : noLink);
  // The arm state: the pilot's own (with landed), else telemetry, where a
  // disarmed vehicle counts as landed and an armed one's landed state is
  // unknown. A blind send on a stale link reads the last known state.
  let armed: boolean;
  let landed: boolean | null;
  if (pilot) {
    armed = pilot.armed;
    landed = pilot.landed;
  } else {
    const known = ctx.live ? ctx.armed : (ctx.lastArmed ?? null);
    if (known === null) return disabled(noLink);
    armed = known;
    landed = armed ? null : true;
  }
  const why = armGateReason(skill.armRequirement, armed, landed);
  return why ? disabled(why) : { enabled: true };
}

function resolveExtension(skill: Skill, ctx: SkillContext): SkillState {
  const now = ctx.now ?? Date.now();
  const reported = ctx.reported?.[skill.id];
  const pending = ctx.pending?.[skill.id] !== undefined;
  const age = reported?.at !== undefined ? now - reported.at : Infinity;
  const stale = reported !== undefined && age > STATE_FRESH_MS;
  const fresh = { stale: stale || undefined, lastSeenS: stale && Number.isFinite(age) ? Math.floor(age / 1000) : undefined };
  const active = reported?.state === "active";
  // A running skill always offers its Stop, whatever the arm gate or the link:
  // on link loss it reads ACTIVE (stale) and its Stop is queued.
  if (active) return { enabled: true, active: true, badge: reported?.badge, pending: pending || undefined, ...fresh };
  if (skill.armRequirement !== "any") {
    const pilot = ctx.pilot ?? null;
    const known = pilot ? pilot.fresh : ctx.fcConnected && ctx.live;
    if (!known) return disabled(ctx.linkReason ?? refusalText("no_fc"));
    const why = armGateReason(skill.armRequirement, pilot ? pilot.armed : ctx.armed, pilot ? pilot.landed : null);
    if (why) return disabled(why);
  }
  if (reported?.state === "disabled" && !stale) return disabled(reported.reason ?? refusalText("bad_state"));
  return { enabled: true, active: false, badge: reported?.badge, pending: pending || undefined, ...fresh };
}

/** Whether a skill can be driven right now, and if not, why. */
export function resolveSkillState(skill: Skill, ctx: SkillContext): SkillState {
  if (skill.builtin) return resolveBuiltin(skill, skill.builtin, ctx);
  if (skill.extension) {
    // Keys, controller bindings and the Launch sheet reach a skill without the
    // picker's filter: a skill for another vehicle class never fires.
    if (!skillVisibleFor(skill, ctx.pilot?.vehicleClass)) return disabled(refusalText("vehicle_class"));
    return resolveExtension(skill, ctx);
  }
  return disabled(refusalText("bad_state"));
}

/** A skill is shown only for the vehicle classes it declares; with no class
 *  reported every skill shows. */
export function skillVisibleFor(skill: Skill, vehicleClass: VehicleClass | null | undefined): boolean {
  if (!skill.extension || !vehicleClass) return true;
  return skill.extension.vehicleClasses.includes(vehicleClass);
}

/** `cyclePalette` → `Cycle palette`, `fire_lrf` → `Fire lrf`. */
function humanise(segment: string): string {
  const words = segment
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/[_-]+/g, " ")
    .trim()
    .toLowerCase();
  return words ? words[0].toUpperCase() + words.slice(1) : segment;
}

const I18N_KEY = /^[A-Za-z][\w-]*(\.[A-Za-z][\w-]*)+$/;

/** A manifest label: a literal, or an i18n key rendered through the plugin's
 *  locale strings, else humanised from its last segment (never a raw key). */
export function displayLabel(raw: string, strings: Readonly<Record<string, string>> | null | undefined): string {
  const t = strings?.[raw];
  if (typeof t === "string" && t) return t;
  if (!I18N_KEY.test(raw)) return raw;
  return humanise(raw.slice(raw.lastIndexOf(".") + 1));
}
