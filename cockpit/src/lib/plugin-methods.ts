// Method → capability rules for the extension bridge. Every method an
// extension can call resolves to exactly one capability the host checks
// against the extension's granted capabilities before any handler runs. A
// method missing from this table is rejected; a `null` capability is always
// allowed (ping, i18n, unsubscribes). Same table Mission Control enforces.

export const PLUGIN_CONFIG_WRITE_COMMAND = "plugin.config.write";
export const VISION_DESIGNATE_COMMAND = "vision.designate";

interface MethodRule {
  capability: string | null;
  resolve?: (args: unknown) => string | null;
  requireTopic?: boolean;
}

const topicOf = (args: unknown): unknown => (args as { topic?: unknown } | null | undefined)?.topic;

export const PLUGIN_METHOD_RULES: Record<string, MethodRule> = {
  ping: { capability: null },
  notify: { capability: "ui.slot.notification-channel" },
  "notification.publish": { capability: "ui.slot.notification-channel" },
  "i18n.t": { capability: null },
  "telemetry.subscribe": {
    capability: "telemetry.subscribe",
    requireTopic: true,
    resolve: (args) => {
      const topic = topicOf(args);
      return typeof topic === "string" ? `telemetry.subscribe.${topic}` : null;
    },
  },
  "telemetry.unsubscribe": { capability: null },
  "command.send": {
    capability: "command.send",
    resolve: (args) =>
      (args as { command?: unknown } | null)?.command === VISION_DESIGNATE_COMMAND
        ? "vision.track.designate"
        : "command.send",
  },
  "recording.start": { capability: "recording.write" },
  "recording.stop": { capability: "recording.write" },
  "recording.mark": { capability: "recording.write" },
  "mission.read": { capability: "mission.read" },
  "mission.write": { capability: "mission.write" },
  "events.subscribe": { capability: "event.subscribe", requireTopic: true },
  "events.publish": { capability: "event.publish", requireTopic: true },
  "events.unsubscribe": { capability: null, requireTopic: true },
  "cloud.read": { capability: "cloud.read" },
  "records.list": { capability: "cloud.records" },
  "records.get": { capability: "cloud.records" },
  "records.put": { capability: "cloud.records" },
  "records.remove": { capability: "cloud.records" },
  "cockpit.marks": { capability: "ui.slot.video-overlay" },
  "cockpit.marks.clear": { capability: null },
  "perception.read": { capability: "perception.read" },
  "perception.subscribe": { capability: "perception.subscribe" },
  "perception.unsubscribe": { capability: null },
  "perception.health": { capability: "perception.read" },
};

/**
 * The capability `method` with `args` requires: null when unrestricted, a
 * capability id when gated, undefined when the method is unknown or its args
 * are malformed (the caller MUST reject).
 */
export function resolveRequiredCapability(method: string, args: unknown): string | null | undefined {
  if (!Object.hasOwn(PLUGIN_METHOD_RULES, method)) return undefined;
  const rule = PLUGIN_METHOD_RULES[method];
  if (rule.requireTopic && typeof topicOf(args) !== "string") return undefined;
  if (!rule.capability) return null;
  return rule.resolve ? rule.resolve(args) : rule.capability;
}

export type BridgeDecision =
  | { ok: true; capability: string | null }
  | { ok: false; code: "method_unknown" | "schema_invalid" | "permission_denied"; message: string };

/** Decide whether an extension holding `granted` may call `method`. */
export function authorizeCall(method: string, args: unknown, granted: readonly string[]): BridgeDecision {
  if (!Object.hasOwn(PLUGIN_METHOD_RULES, method)) {
    return { ok: false, code: "method_unknown", message: `unknown method ${method}` };
  }
  const required = resolveRequiredCapability(method, args);
  if (required === undefined) {
    return { ok: false, code: "schema_invalid", message: `bad args for ${method}` };
  }
  if (required !== null && !granted.includes(required)) {
    return { ok: false, code: "permission_denied", message: `plugin lacks capability ${required}` };
  }
  return { ok: true, capability: required };
}
