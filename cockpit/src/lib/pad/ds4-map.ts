// Typed access to the controller map, `cockpit-spec/ds4.json` (synced from
// web-shared): the controls, the default binding rows, the stick modes, the
// conditioning and stream constants, the rumble patterns and the action table
// the binding editor offers. The same file drives Mission Control, Android and
// the ground station's native input, so this module only narrows types; it
// never restates a value.
//
// Binding rows name actions by the spec's ids (`zoom_out`, `skill_select`);
// `cockpitActionId` turns one into the cockpit action registry id the row
// fires (`camera.zoom_out`, `skill.go`). A row may also name a registry id the
// spec does not list (an installed skill, `skill:<plugin>:<skill>`), which
// fires as is.

import ds4 from "@/shared/cockpit-spec/ds4.json";
import layout from "@/shared/cockpit-spec/layout.json";

export type ControlId =
  | "stick_l"
  | "stick_r"
  | "l1"
  | "r1"
  | "l2"
  | "r2"
  | "l3"
  | "r3"
  | "dpad_up"
  | "dpad_down"
  | "dpad_left"
  | "dpad_right"
  | "cross"
  | "circle"
  | "square"
  | "triangle"
  | "share"
  | "options"
  | "ps"
  | "touchpad"
  | "lightbar";

export type Gesture = "press" | "tap" | "hold" | "double_tap" | "analog";

export type WhenCondition =
  | "picker_open"
  | "picker_closed"
  | "on_ground"
  | "airborne"
  | "rth_active"
  | "not_rth"
  | "stick_l_centered"
  | "vehicle_vtol";

export type RumbleKind = "mode_change" | "warning" | "critical" | "ramp" | "countdown";

export type StickMode = "mode1" | "mode2";

export type ActionGroup = "flight" | "skills" | "camera" | "view" | "profile" | "none";

/** One binding row, shaped exactly like `ds4.json` `bindings[]` (and the
 *  export file / ground-station PUT body). */
export interface PadBinding {
  control: ControlId;
  with: ControlId[];
  gesture: Gesture;
  hold_ms: number | null;
  when: WhenCondition | null;
  action: string;
  rumble: RumbleKind | null;
  locked: boolean;
  rate_full_dps?: number;
}

export interface SpecAction {
  id: string;
  label: string;
  group: ActionGroup;
  builtin: string | null;
}

export interface LockedAction {
  action: string;
  gesture: Gesture;
  combo: boolean;
}

export interface StickAxisSpec {
  stick: "stick_l" | "stick_r";
  axis: "x" | "y";
  invert: boolean;
}

export type StickModeSpec = Record<"throttle" | "yaw" | "pitch" | "roll", StickAxisSpec>;

export const CONTROL_IDS = ds4.controls as readonly ControlId[];
export const BUTTON_CONTROLS: readonly ControlId[] = CONTROL_IDS.filter(
  (c) => c !== "stick_l" && c !== "stick_r" && c !== "lightbar",
);

export const DEFAULT_STICK_MODE = ds4.default_mode as StickMode;
export const STICK_MODES = ds4.stick_modes as Record<StickMode, StickModeSpec>;

export const DEADZONE_DEFAULT = ds4.conditioning.radial_deadzone;
export const DEADZONE_MIN = ds4.conditioning.deadzone_min;
export const DEADZONE_MAX = ds4.conditioning.deadzone_max;

export const STREAM_RATE_HZ = ds4.stream.rate_hz;
export const STREAM_INTERVAL_MS = Math.round(1000 / ds4.stream.rate_hz);
export const STALE_SAMPLE_MS = ds4.stream.stale_timestamp_ms;
export const TRIGGER_PRESSED = ds4.stream.trigger_pressed_threshold;
export const CENTERED_THRESHOLD = ds4.centered_threshold;
export const DOUBLE_TAP_MS = ds4.double_tap_ms;

/** The emergency stop's hold (`layout.json` `holds_ms.stop`). A hold row
 *  bound to the stop never completes sooner, whatever its own `hold_ms`. */
export const STOP_HOLD_MS: number = layout.holds_ms.stop;
export const STOP_ACTION = "kill";

/** The hold a row actually needs: its own, raised to the stop hold for an
 *  emergency-stop row. Null for a row with no hold. */
export function effectiveHoldMs(b: Pick<PadBinding, "action" | "hold_ms">): number | null {
  if (b.hold_ms == null) return null;
  return b.action === STOP_ACTION ? Math.max(b.hold_ms, STOP_HOLD_MS) : b.hold_ms;
}

/** The W3C standard-mapping button index of every button control (the same
 *  numbers as the stick stream's button bits). */
export const BUTTON_INDEX = ds4.platform_map.web_standard.buttons as Readonly<Partial<Record<ControlId, number>>>;
export const BUTTON_BITS = ds4.stream.buttons_bitfield as Readonly<Partial<Record<ControlId, number>>>;
export const STANDARD_AXES = ds4.platform_map.web_standard.axes as Readonly<
  Record<"stick_l_x" | "stick_l_y" | "stick_r_x" | "stick_r_y", number>
>;

export const SPEC_ACTIONS = ds4.actions as readonly SpecAction[];
export const SPEC_ACTION_BY_ID: Readonly<Record<string, SpecAction>> = Object.fromEntries(
  SPEC_ACTIONS.map((a) => [a.id, a]),
);
export const ACTION_GROUPS = ds4.action_groups as readonly ActionGroup[];

export const DEFAULT_BINDINGS = ds4.bindings as readonly PadBinding[];
export const LOCKED_ACTIONS = ds4.locked_actions as readonly LockedAction[];
export const WHEN_CONDITIONS = ds4.when_conditions as Readonly<Record<WhenCondition, string>>;

export interface RumblePulses {
  pulses_ms: number[];
  gap_ms: number;
  strong: number;
  weak: number;
}

export const RUMBLE = ds4.rumble as {
  mode_change: RumblePulses;
  warning: RumblePulses;
  critical: RumblePulses;
  ramp: { from: number; to: number; tick_ms: number };
  countdown: { pulse_ms: number; every_ms: number };
};

export interface LightbarState {
  color: string;
  pattern: "solid" | "blink" | "pulse";
  hz: number | null;
}
export const LIGHTBAR_STATES = ds4.lightbar.states as Record<string, LightbarState>;

/** The export file's `kind`. */
export const EXPORT_KIND = ds4.export_format.example.kind;
export const EXPORT_VERSION = ds4.v;

/** Spec action id → the cockpit action registry id it fires. Ids absent here
 *  fire under their own name (the flight built-ins, `profile:*`,
 *  `quick_settings`, `designate`, installed skills). */
const COCKPIT_ID: Readonly<Record<string, string>> = {
  rth_cancel: "pause",
  vtol_transition: "vtol.transition",
  skill_select: "skill.go",
  skill_stop: "skill.stop",
  picker_focus_prev: "picker.left",
  picker_focus_next: "picker.right",
  param_up: "param.up",
  param_down: "param.down",
  photo: "camera.photo",
  record_toggle: "camera.record",
  zoom_in: "camera.zoom_in",
  zoom_out: "camera.zoom_out",
  gimbal_recenter: "gimbal.recenter",
  view_next: "view.next",
  swap_video_map: "swap_map",
  layer_level_up: "layer.level.next",
  layer_level_down: "layer.level.prev",
  profile_prev: "profile.prev",
  profile_next: "profile.next",
};

/** The analog actions: they drive a rate, not a registry action. */
export const ANALOG_ACTIONS: Readonly<Record<string, true>> = { gimbal_pitch_up: true, gimbal_pitch_down: true };

export function cockpitActionId(action: string): string | null {
  if (action === "none" || ANALOG_ACTIONS[action]) return null;
  return COCKPIT_ID[action] ?? action;
}

/** The registry ids a spec action already covers; the editor's picker lists
 *  the spec entry and hides the duplicate registry entry. */
export const COVERED_COCKPIT_IDS: ReadonlySet<string> = new Set(
  SPEC_ACTIONS.map((a) => cockpitActionId(a.id)).filter((id): id is string => id !== null),
);

export function isControlId(v: unknown): v is ControlId {
  return typeof v === "string" && (CONTROL_IDS as readonly string[]).includes(v);
}

const GESTURES: readonly Gesture[] = ["press", "tap", "hold", "double_tap", "analog"];
export function isGesture(v: unknown): v is Gesture {
  return typeof v === "string" && (GESTURES as readonly string[]).includes(v);
}

export function isWhenCondition(v: unknown): v is WhenCondition {
  return typeof v === "string" && v in WHEN_CONDITIONS;
}

const RUMBLES: readonly RumbleKind[] = ["mode_change", "warning", "critical", "ramp", "countdown"];
export function isRumbleKind(v: unknown): v is RumbleKind {
  return typeof v === "string" && (RUMBLES as readonly string[]).includes(v);
}

/** Display names of the controls, for the editor and the leader labels. */
export const CONTROL_LABELS: Readonly<Record<ControlId, string>> = {
  stick_l: "Left stick",
  stick_r: "Right stick",
  l1: "L1",
  r1: "R1",
  l2: "L2",
  r2: "R2",
  l3: "L3",
  r3: "R3",
  dpad_up: "D-pad ↑",
  dpad_down: "D-pad ↓",
  dpad_left: "D-pad ←",
  dpad_right: "D-pad →",
  cross: "Cross",
  circle: "Circle",
  square: "Square",
  triangle: "Triangle",
  share: "Share",
  options: "Options",
  ps: "PS",
  touchpad: "Touchpad",
  lightbar: "Lightbar",
};
