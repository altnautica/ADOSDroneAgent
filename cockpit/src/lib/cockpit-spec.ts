// Typed access to the cockpit spec (web-shared `cockpit-spec/`, synced into
// `src/shared/cockpit-spec/`): the built-in skills, the layout, the views, the
// flight-state chip rules and the colour tokens. Every surface reads the same
// files; this module is the one place the cockpit turns them into typed values.

import builtinsJson from "@/shared/cockpit-spec/builtins.json";
import flightStateJson from "@/shared/cockpit-spec/flight-state.json";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import tokensJson from "@/shared/cockpit-spec/tokens.json";

export type BuiltinArmRequirement = "any" | "disarmed" | "armed" | "armed_landed" | "disarmed_or_landed";

export interface BuiltinConfirm {
  /** `tap` fires at once; `hold` needs `ms` of press; `hold_confirm` is a
   *  hold followed by an explicit confirm (kill). */
  kind: "tap" | "hold" | "hold_confirm";
  ms: number;
}

export interface BuiltinSpec {
  id: string;
  label: string;
  icon: string;
  armRequirement: BuiltinArmRequirement;
  confirm: BuiltinConfirm;
  needsFreshLink: boolean;
  /** On a stale link: disabled with a reason, or sent blind. */
  staleLink: "disable" | "send_blind";
  /** Keyboard chord (`shift+a`, `space`), or null. */
  keyboard: string | null;
  armsFirst: boolean;
  /** The pilot action name (pilot.v1 `action`). */
  pilotAction: string;
}

interface RawBuiltin {
  id: string;
  label: string;
  icon: string;
  arm_requirement: string;
  confirm: { kind: string; ms: number };
  needs_fresh_link: boolean;
  stale_link: string;
  keyboard: string | null;
  arms_first: boolean;
  pilot_action: string;
}

export const BUILTINS: readonly BuiltinSpec[] = (builtinsJson.builtins as RawBuiltin[]).map((b) => ({
  id: b.id,
  label: b.label,
  icon: b.icon,
  armRequirement: b.arm_requirement as BuiltinArmRequirement,
  confirm: { kind: b.confirm.kind as BuiltinConfirm["kind"], ms: b.confirm.ms },
  needsFreshLink: b.needs_fresh_link,
  staleLink: b.stale_link === "send_blind" ? "send_blind" : "disable",
  keyboard: b.keyboard,
  armsFirst: b.arms_first,
  pilotAction: b.pilot_action,
}));

export const BUILTIN_BY_ID: Readonly<Record<string, BuiltinSpec>> = Object.fromEntries(
  BUILTINS.map((b) => [b.id, b]),
);

/** Profile keys (`f1` → `cine`, …). */
export const PROFILE_KEYS: Readonly<Record<string, string>> = builtinsJson.profile_keys as Record<string, string>;

// ---- flight-state chip (rules in `@/shared/hud/flight-state`) -----------------

export const MODE_SHEET = flightStateJson.mode_sheet;

/** A state colour name (`pilot`, `mission`, …) as a CSS colour. */
export const STATE_COLORS: Readonly<Record<string, string>> = tokensJson.state as Record<string, string>;

// ---- layout ------------------------------------------------------------------

export interface WidthClassSpec {
  id: "compact" | "medium" | "expanded" | "tv";
  min: number;
  max: number | null;
}

export const WIDTH_CLASSES: readonly WidthClassSpec[] = layoutJson.breakpoints.classes as WidthClassSpec[];

export interface LayoutClassSpec {
  column_width: number;
  bottom_left_card: { kind: "chip" | "card"; size?: number; width?: number; height?: number };
  labels: "distance_only" | "full";
  default_layer_level: "full" | "route" | "off";
  scale: number;
}

export const LAYOUT_CLASSES = layoutJson.classes as unknown as Record<
  "compact" | "medium" | "expanded" | "tv",
  LayoutClassSpec
>;

export const TOUCH_TARGET = layoutJson.touch_target;
export const BANNERS = layoutJson.banners;
export const ON_SCREEN_STICKS = layoutJson.on_screen_sticks;
