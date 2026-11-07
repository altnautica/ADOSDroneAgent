// Installed extensions as the cockpit sees them: the enabled installs from
// `GET /api/plugins`, each with its detail (`GET /api/plugins/{id}`: the GCS
// half's entrypoint and contributions, and the granted capabilities) and its
// locale strings, plus the parsers that turn those contributions into plugin
// skills (strictly validated against the manifest skill schema), their
// Tweak parameters and quick settings, target actions for the Feed's target
// menu, and mountable panels.

import {
  PLUGIN_CONFIRM_MS,
  VEHICLE_CLASSES,
  displayLabel,
  type ArmRequirement,
  type ParamValue,
  type PluginSkillCategory,
  type ReportedSkillState,
  type Skill,
  type SkillParam,
  type SkillSetting,
} from "@/lib/skills";
import type { VehicleClass } from "@/shared/hud/cockpit-frames";

export interface ExtensionPanel {
  id: string;
  slot: string;
  title: string;
  zone: string | null;
}

/** A manifest-declared action on a target picked in the Feed's target layer
 *  (`gcs.contributes.target_actions`). The host designates the box when
 *  `designate` is set, then writes `configKey = configValue` on the plugin. */
export interface TargetAction {
  pluginId: string;
  id: string;
  label: string;
  order: number;
  /** Offered only for detections of this class (case-insensitive), or any
   *  class when null. */
  appliesToClass: string | null;
  designate: boolean;
  configKey: string | null;
  configValue: unknown;
}

/** A manifest skill the cockpit refused, with every reason, for the
 *  Settings › Controls conflicts view. */
export interface RejectedSkill {
  pluginId: string;
  pluginName: string;
  /** The declared id, or null when it had none. */
  skillId: string | null;
  reasons: string[];
}

export interface InstalledExtension {
  pluginId: string;
  name: string;
  entrypoint: string | null;
  isolation: "iframe" | "inline";
  grantedCapabilities: string[];
  panels: ExtensionPanel[];
  skills: Skill[];
  rejected: RejectedSkill[];
  targetActions: TargetAction[];
  /** Locales the GCS half ships (`gcs.locales`). */
  locales: string[];
}

/** The slot capability each cockpit mount needs. */
export const SLOT_CAPABILITY: Record<string, string> = {
  "video.overlay": "ui.slot.video-overlay",
  "cockpit.widget": "ui.slot.cockpit-widget",
  "cockpit.panel": "ui.slot.cockpit-panel",
};

/** Panels of `slot` the extension is allowed to mount. */
export function mountablePanels(ext: InstalledExtension, slot: string): ExtensionPanel[] {
  const cap = SLOT_CAPABILITY[slot];
  if (!cap || !ext.grantedCapabilities.includes(cap)) return [];
  return ext.panels.filter((p) => p.slot === slot);
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
function str(v: unknown): string | null {
  return typeof v === "string" && v ? v : null;
}
function list(v: unknown): unknown[] {
  return Array.isArray(v) ? v : [];
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Whether an install row is running (the states the agent reports for a
 *  started plugin). */
export function isEnabledInstall(status: unknown): boolean {
  return status === "enabled" || status === "running";
}

const KEBAB = /^[a-z0-9]+(-[a-z0-9]+)*$/;
const CATEGORIES: readonly PluginSkillCategory[] = ["behavior", "camera", "navigation", "utility"];
const ARM_REQUIREMENTS: readonly ArmRequirement[] = ["any", "armed", "disarmed"];

export interface SkillParseContext {
  pluginName: string;
  /** The plugin's locale strings, for i18n-key labels. */
  strings?: Readonly<Record<string, string>> | null;
  /** The plugin's parameters by key, for the skill's `params`. */
  parameters?: ReadonlyMap<string, SkillParam>;
}

export type SkillParseResult = { ok: true; skill: Skill } | { ok: false; rejected: RejectedSkill };

/**
 * Validate one `contributes.skills[]` entry against the manifest skill schema
 * and build the skill. Required: `id` (kebab case), `label`, `icon`,
 * `category` ∈ behavior|camera|navigation|utility, `toggle` and `confirm`
 * booleans, `arm_requirement` ∈ any|armed|disarmed, `activation {via:
 * "config", config_key}` and `state {via: "event", topic}`. The optional
 * `motion`, `nudge`, `one_shot`, `params`, `vehicle_classes` and
 * `default_binding` must have their declared types. Anything else rejects the
 * skill with every reason found.
 */
export function parseExtensionSkill(pluginId: string, raw: unknown, ctx: SkillParseContext): SkillParseResult {
  const reasons: string[] = [];
  const reject = (skillId: string | null): SkillParseResult => ({
    ok: false,
    rejected: { pluginId, pluginName: ctx.pluginName, skillId, reasons },
  });
  if (!isObj(raw)) {
    reasons.push("not an object");
    return reject(null);
  }
  const id = str(raw.id);
  if (!id) reasons.push("missing id");
  else if (!KEBAB.test(id)) reasons.push(`id "${id}" is not kebab case`);
  const label = str(raw.label);
  if (!label) reasons.push("missing label");
  const icon = str(raw.icon);
  if (!icon) reasons.push("missing icon");
  const category = CATEGORIES.find((c) => c === raw.category);
  if (!category) reasons.push(`category must be one of ${CATEGORIES.join(", ")}`);
  if (typeof raw.toggle !== "boolean") reasons.push("toggle must be a boolean");
  if (typeof raw.confirm !== "boolean") reasons.push("confirm must be a boolean");
  const arm = ARM_REQUIREMENTS.find((a) => a === raw.arm_requirement);
  if (!arm) reasons.push(`arm_requirement must be one of ${ARM_REQUIREMENTS.join(", ")}`);
  const activation = isObj(raw.activation) ? raw.activation : null;
  const configKey = activation ? str(activation.config_key) : null;
  if (!activation || activation.via !== "config") reasons.push('activation.via must be "config"');
  if (!configKey) reasons.push("missing activation.config_key");
  const state = isObj(raw.state) ? raw.state : null;
  const topic = state ? str(state.topic) : null;
  if (!state || state.via !== "event") reasons.push('state.via must be "event"');
  if (!topic) reasons.push("missing state.topic");

  const optBool = (key: string): boolean => {
    const v = raw[key];
    if (v === undefined) return false;
    if (typeof v !== "boolean") reasons.push(`${key} must be a boolean`);
    return v === true;
  };
  const motion = optBool("motion");
  const nudge = optBool("nudge");
  const oneShot = optBool("one_shot");

  let paramKeys: string[] = [];
  if (raw.params !== undefined) {
    if (!Array.isArray(raw.params) || !raw.params.every((p) => typeof p === "string" && p)) {
      reasons.push("params must be a list of parameter keys");
    } else {
      paramKeys = raw.params as string[];
      const unknown = ctx.parameters ? paramKeys.filter((k) => !ctx.parameters!.has(k)) : [];
      if (unknown.length > 0) reasons.push(`params name undeclared parameters: ${unknown.join(", ")}`);
    }
  }

  let vehicleClasses: readonly VehicleClass[] = VEHICLE_CLASSES;
  if (raw.vehicle_classes !== undefined) {
    const vc = raw.vehicle_classes;
    if (
      !Array.isArray(vc) ||
      vc.length === 0 ||
      !vc.every((c) => (VEHICLE_CLASSES as readonly unknown[]).includes(c))
    ) {
      reasons.push(`vehicle_classes must list ${VEHICLE_CLASSES.join(", ")}`);
    } else {
      vehicleClasses = vc as VehicleClass[];
    }
  }

  let defaultBinding: { key: string | null; gamepadButton: number | string | null } | null = null;
  if (raw.default_binding !== undefined) {
    const db = raw.default_binding;
    const key = isObj(db) ? db.key : undefined;
    const pad = isObj(db) ? db.gamepad_button : undefined;
    const keyOk = key === undefined || (typeof key === "string" && key !== "");
    const padOk = pad === undefined || (typeof pad === "string" && pad !== "") || (typeof pad === "number" && Number.isInteger(pad) && pad >= 0);
    if (!isObj(db) || !keyOk || !padOk) {
      reasons.push("default_binding must be {key?, gamepad_button?}");
    } else {
      defaultBinding = {
        key: typeof key === "string" ? key : null,
        gamepadButton: typeof pad === "string" || typeof pad === "number" ? pad : null,
      };
    }
  }

  if (reasons.length > 0 || !id || !label || !category || !arm || !configKey || !topic) return reject(id);
  const params = paramKeys.map((k) => ctx.parameters?.get(k)).filter((p): p is SkillParam => p !== undefined);
  return {
    ok: true,
    skill: {
      id: `${pluginId}:${id}`,
      label: displayLabel(label, ctx.strings),
      category,
      confirm: raw.confirm === true ? { kind: "hold", ms: PLUGIN_CONFIRM_MS } : { kind: "tap", ms: 0 },
      armRequirement: arm,
      icon: icon ?? undefined,
      params: params.length > 0 ? params : undefined,
      extension: {
        pluginId,
        pluginName: ctx.pluginName,
        localId: id,
        configKey,
        toggle: raw.toggle === true,
        stateTopic: topic,
        motion,
        nudge,
        oneShot,
        vehicleClasses,
        defaultBinding,
      },
    },
  };
}

function scalar(v: unknown): ParamValue | null {
  return typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v)) ? v : null;
}

/** The plugin's `gcs.contributes.parameters` written through the plugin
 *  config (`binding: plugin.config`) that a Tweak panel can drive: numbers
 *  (stepper and slider), string enums (segmented) and booleans. */
export function parseSkillParams(
  raw: unknown,
  strings?: Readonly<Record<string, string>> | null,
): Map<string, SkillParam> {
  const out = new Map<string, SkillParam>();
  for (const p of list(raw)) {
    if (!isObj(p) || p.binding !== "plugin.config") continue;
    const key = str(p.key);
    const schema = isObj(p.schema) ? p.schema : null;
    if (!key || !schema) continue;
    const ui = isObj(p.ui) ? p.ui : {};
    const vi = isObj(ui.visible_if) ? ui.visible_if : null;
    const viKey = vi ? str(vi.key) : null;
    const viEq = vi ? scalar(vi.equals) : null;
    const base = {
      key,
      label: displayLabel(str(ui.label) ?? key, strings),
      visibleIf: viKey && viEq !== null ? { key: viKey, equals: viEq } : null,
    };
    const values = list(schema.enum).filter((v): v is string => typeof v === "string");
    if (values.length > 0) {
      out.set(key, { ...base, kind: "enum", values, defaultValue: str(schema.default) });
    } else if (schema.type === "number" || schema.type === "integer") {
      const min = num(schema.minimum);
      const max = num(schema.maximum);
      if (min === null || max === null || max <= min) continue;
      const step = num(schema.step) ?? (schema.type === "integer" ? 1 : (max - min) / 100);
      out.set(key, { ...base, kind: "number", min, max, step, defaultValue: num(schema.default) });
    } else if (schema.type === "boolean") {
      out.set(key, { ...base, kind: "boolean", defaultValue: typeof schema.default === "boolean" ? schema.default : null });
    }
  }
  return out;
}

/** Whether a parameter shows under the current values (`visible_if`). */
export function paramVisible(param: SkillParam, valueOf: (key: string) => ParamValue | null): boolean {
  return param.visibleIf === null || valueOf(param.visibleIf.key) === param.visibleIf.equals;
}

/** The plugin's two-value enum parameters, offered as a segmented quick
 *  setting on its toggle skills (follow-me's `reacquire_mode`, STRICT | SMART). */
export function parseSkillSettings(params: ReadonlyMap<string, SkillParam>): SkillSetting[] {
  const out: SkillSetting[] = [];
  for (const p of params.values()) {
    if (p.kind !== "enum" || p.values.length !== 2) continue;
    out.push({ key: p.key, values: [p.values[0], p.values[1]], defaultValue: p.defaultValue });
  }
  return out;
}

/** Parse `gcs.contributes.target_actions`, ordered by `order` then id. */
export function parseTargetActions(pluginId: string, raw: unknown): TargetAction[] {
  const out: TargetAction[] = [];
  for (const a of list(raw)) {
    if (!isObj(a)) continue;
    const id = str(a.id);
    if (!id) continue;
    out.push({
      pluginId,
      id,
      label: str(a.label) ?? id,
      order: typeof a.order === "number" ? a.order : 0,
      appliesToClass: str(a.applies_to_class),
      designate: a.designate === true,
      configKey: str(a.config_key),
      configValue: a.config_value,
    });
  }
  return out.sort((x, y) => x.order - y.order || x.id.localeCompare(y.id));
}

/** The locales a plugin detail's GCS half declares (`gcs.locales`). */
export function detailLocales(detail: unknown): string[] {
  const d = isObj(detail) ? detail : {};
  const manifest = isObj(d.manifest) ? d.manifest : {};
  const gcs = isObj(manifest.gcs) ? manifest.gcs : {};
  return list(gcs.locales).filter((l): l is string => typeof l === "string" && l !== "");
}

/** The locale to render a plugin in: the browser language when the plugin
 *  ships it, else English, else its first. Null when it ships none. */
export function pickLocale(locales: readonly string[], preferred: string | null): string | null {
  const base = preferred?.toLowerCase().split("-")[0] ?? null;
  return (
    locales.find((l) => l.toLowerCase() === preferred?.toLowerCase()) ??
    locales.find((l) => l.toLowerCase() === base) ??
    locales.find((l) => l.toLowerCase() === "en") ??
    locales[0] ??
    null
  );
}

/** Parse the plugin detail body (and its locale strings) into the cockpit's
 *  view of the extension. */
export function parseExtensionDetail(
  pluginId: string,
  detail: unknown,
  strings: Readonly<Record<string, string>> | null = null,
): InstalledExtension {
  const d = isObj(detail) ? detail : {};
  const manifest = isObj(d.manifest) ? d.manifest : {};
  const gcs = isObj(manifest.gcs) ? manifest.gcs : null;
  const contributes = gcs && isObj(gcs.contributes) ? gcs.contributes : {};
  const panels: ExtensionPanel[] = [];
  for (const raw of list(contributes.panels)) {
    if (!isObj(raw)) continue;
    const id = str(raw.id);
    const slot = str(raw.slot);
    if (!id || !slot) continue;
    panels.push({ id, slot, title: str(raw.title) ?? id, zone: str(raw.zone) });
  }
  const name = str(manifest.name) ?? pluginId;
  const parameters = parseSkillParams(contributes.parameters, strings);
  const settings = parseSkillSettings(parameters);
  const skills: Skill[] = [];
  const rejected: RejectedSkill[] = [];
  const seen = new Set<string>();
  for (const raw of list(contributes.skills)) {
    const r = parseExtensionSkill(pluginId, raw, { pluginName: name, strings, parameters });
    if (!r.ok) {
      rejected.push(r.rejected);
    } else if (seen.has(r.skill.id)) {
      rejected.push({ pluginId, pluginName: name, skillId: r.skill.extension!.localId, reasons: ["duplicate id"] });
    } else {
      seen.add(r.skill.id);
      skills.push(r.skill.extension!.toggle && settings.length > 0 ? { ...r.skill, settings } : r.skill);
    }
  }
  return {
    pluginId,
    name,
    entrypoint: gcs ? str(gcs.entrypoint) : null,
    isolation: gcs?.isolation === "inline" ? "inline" : "iframe",
    grantedCapabilities: list(d.granted_capabilities).filter((c): c is string => typeof c === "string"),
    panels,
    skills,
    rejected,
    targetActions: parseTargetActions(pluginId, contributes.target_actions),
    locales: detailLocales(detail),
  };
}

/**
 * Map a plugin's published state payload for a skill topic to the discrete
 * state the picker shows. An explicit `state` string wins; a
 * `{active, commanding, lock_state}` payload reads active (with a lock badge
 * while engaged but not commanding); anything else reads idle, never an
 * optimistic active.
 */
export function mapReportedState(payload: unknown): ReportedSkillState {
  if (!isObj(payload)) return { state: "idle" };
  if (payload.state === "active" || payload.state === "idle" || payload.state === "disabled") {
    return {
      state: payload.state,
      badge: str(payload.badge) ?? undefined,
      reason: str(payload.reason) ?? undefined,
    };
  }
  if (typeof payload.active === "boolean") {
    if (!payload.active) return { state: "idle" };
    if (payload.commanding === true) return { state: "active" };
    const lock = str(payload.lock_state);
    return lock ? { state: "active", badge: lock.slice(0, 4).toUpperCase() } : { state: "active" };
  }
  return { state: "idle" };
}

/** The reported state of every skill of one extension from its state body
 *  (`{ topic: { payload, ts_ms } }`), keyed by skill id, stamped `at` (local
 *  epoch ms of receipt). A skill whose topic is absent is left out, so its
 *  last state ages. Its quick settings and parameters read their applied
 *  values from the same payload (the plugin echoes the keys it runs with). */
export function reportedStatesFor(
  ext: InstalledExtension,
  body: unknown,
  at: number,
): Record<string, ReportedSkillState> {
  const out: Record<string, ReportedSkillState> = {};
  const topics = isObj(body) ? body : {};
  for (const skill of ext.skills) {
    const topic = skill.extension?.stateTopic;
    if (!topic || !isObj(topics[topic])) continue;
    const payload = (topics[topic] as Record<string, unknown>).payload;
    const state = mapReportedState(payload);
    const keys = [...(skill.settings ?? []).map((s) => s.key), ...(skill.params ?? []).map((p) => p.key)];
    if (keys.length > 0 && isObj(payload)) {
      const echo: Record<string, ParamValue> = {};
      for (const k of keys) {
        const v = scalar(payload[k]);
        if (v !== null) echo[k] = v;
      }
      state.echo = echo;
    }
    state.at = at;
    out[skill.id] = state;
  }
  return out;
}
