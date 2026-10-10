// Installed extensions as the cockpit sees them: the enabled installs from
// `GET /api/plugins`, each with its detail (`GET /api/plugins/{id}`: the GCS
// half's entrypoint and contributions, and the granted capabilities), plus the
// parsers that turn those contributions into Skill Bar skills and mountable
// panels.

import type { ReportedSkillState, Skill } from "@/lib/skills";

export interface ExtensionPanel {
  id: string;
  slot: string;
  title: string;
  zone: string | null;
}

export interface InstalledExtension {
  pluginId: string;
  name: string;
  entrypoint: string | null;
  isolation: "iframe" | "inline";
  grantedCapabilities: string[];
  panels: ExtensionPanel[];
  skills: Skill[];
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

/** Whether an install row is running (the states the agent reports for a
 *  started plugin). */
export function isEnabledInstall(status: unknown): boolean {
  return status === "enabled" || status === "running";
}

/** Parse one `contributes.skills[]` entry into a Skill, or null when it does
 *  not activate through a config key (the only activation this host drives). */
export function parseExtensionSkill(pluginId: string, raw: unknown): Skill | null {
  if (!isObj(raw)) return null;
  const localId = str(raw.id);
  const activation = isObj(raw.activation) ? raw.activation : {};
  const state = isObj(raw.state) ? raw.state : {};
  const configKey = str(activation.config_key);
  if (!localId || !configKey || (activation.via !== undefined && activation.via !== "config")) {
    return null;
  }
  const arm = raw.arm_requirement;
  return {
    id: `${pluginId}:${localId}`,
    label: str(raw.label) ?? localId,
    category: "extension",
    gesture: raw.confirm === true ? "hold" : "tap",
    armRequirement: arm === "armed" || arm === "disarmed" ? arm : "any",
    icon: str(raw.icon) ?? undefined,
    extension: {
      pluginId,
      configKey,
      toggle: raw.toggle === true,
      stateTopic: str(state.topic),
    },
  };
}

/** Parse the plugin detail body into the cockpit's view of the extension. */
export function parseExtensionDetail(pluginId: string, detail: unknown): InstalledExtension {
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
  return {
    pluginId,
    name: str(manifest.name) ?? pluginId,
    entrypoint: gcs ? str(gcs.entrypoint) : null,
    isolation: gcs?.isolation === "inline" ? "inline" : "iframe",
    grantedCapabilities: list(d.granted_capabilities).filter(
      (c): c is string => typeof c === "string",
    ),
    panels,
    skills: list(contributes.skills)
      .map((raw) => parseExtensionSkill(pluginId, raw))
      .filter((s): s is Skill => s !== null),
  };
}

/**
 * Map a plugin's published state payload for a skill topic to the discrete
 * state the Skill Bar shows. An explicit `state` string wins; a
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
 *  (`{ topic: { payload, ts_ms } }`), keyed by skill id. */
export function reportedStatesFor(
  ext: InstalledExtension,
  body: unknown,
): Record<string, ReportedSkillState> {
  const out: Record<string, ReportedSkillState> = {};
  const topics = isObj(body) ? body : {};
  for (const skill of ext.skills) {
    const topic = skill.extension?.stateTopic;
    if (!topic) continue;
    const entry = topics[topic];
    out[skill.id] = mapReportedState(isObj(entry) ? entry.payload : undefined);
  }
  return out;
}
