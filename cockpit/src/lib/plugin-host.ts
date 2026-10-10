// The cockpit side of one extension frame's bridge. Every request is checked
// against the extension's granted capabilities (`authorizeCall`) before a
// handler runs. Telemetry and perception answers come from the cockpit's own
// live stores, never from constants; a method this host cannot serve answers
// `unsupported_on_platform` rather than hanging the caller.

import type { InstalledExtension } from "@/lib/extensions";
import { PLUGIN_CONFIG_WRITE_COMMAND, authorizeCall } from "@/lib/plugin-methods";
import type { VehicleState } from "@/lib/types";
import { apiFetch } from "@/shared/api-fetch";
import { useDetectionsStore, type CockpitDetectionBatch } from "@/stores/detections-store";
import { useFlightStore } from "@/stores/flight-store";

export const PROTOCOL_VERSION = 1;

export interface RpcEnvelope {
  id?: string;
  type?: "request" | "response" | "event";
  method?: string;
  capability?: string;
  args?: unknown;
  version?: number;
  error?: { code: string; message: string };
}

const RAD_TO_DEG = 180 / Math.PI;

/** Telemetry topics this host serves, each projected from the vehicle
 *  snapshot into the shape the extension SDK documents. Null when the snapshot
 *  carries nothing for the topic (no frame is sent). */
const TOPICS: Record<string, (t: VehicleState, ts: number) => unknown | null> = {
  attitude: (t, ts) =>
    t.attitude && Number.isFinite(t.attitude.roll) && Number.isFinite(t.attitude.pitch)
      ? {
          timestamp: ts,
          roll: (t.attitude.roll as number) * RAD_TO_DEG,
          pitch: (t.attitude.pitch as number) * RAD_TO_DEG,
          yaw: (t.attitude.yaw ?? 0) * RAD_TO_DEG,
        }
      : null,
  position: (t, ts) =>
    t.position && Number.isFinite(t.position.lat) && Number.isFinite(t.position.lon)
      ? {
          timestamp: ts,
          lat: t.position.lat,
          lon: t.position.lon,
          alt: t.position.alt_msl ?? 0,
          relativeAlt: t.position.alt_rel ?? undefined,
          heading: t.position.heading ?? undefined,
          groundSpeed: t.velocity?.groundspeed ?? 0,
          airSpeed: t.velocity?.airspeed ?? undefined,
          climbRate: t.velocity?.climb ?? undefined,
          vn: t.velocity?.vx ?? undefined,
          ve: t.velocity?.vy ?? undefined,
          vd: t.velocity?.vz ?? undefined,
        }
      : null,
  gps: (t, ts) =>
    t.gps
      ? {
          timestamp: ts,
          fixType: t.gps.fix_type ?? 0,
          satellites: t.gps.satellites ?? undefined,
          hdop: t.gps.eph ?? undefined,
          lat: t.position?.lat ?? 0,
          lon: t.position?.lon ?? 0,
          alt: t.position?.alt_msl ?? 0,
        }
      : null,
  vfr: (t, ts) =>
    t.velocity
      ? {
          timestamp: ts,
          airspeed: t.velocity.airspeed ?? undefined,
          groundspeed: t.velocity.groundspeed ?? undefined,
          heading: t.position?.heading ?? undefined,
          throttle: t.throttle ?? undefined,
          alt: t.position?.alt_rel ?? 0,
          climb: t.velocity.climb ?? 0,
        }
      : null,
  battery: (t, ts) =>
    t.battery
      ? {
          timestampMs: ts,
          packId: 0,
          cellVoltagesV: [],
          totalVoltageV: t.battery.voltage ?? 0,
          currentA: t.battery.current,
          consumedAh: null,
          remainingPercent:
            typeof t.battery.remaining === "number" && t.battery.remaining >= 0 ? t.battery.remaining : null,
          temperatureC: t.battery.temperature,
          cellCount: null,
        }
      : null,
  heartbeat: (t, ts) =>
    t.mode != null ? { timestamp: ts, armed: t.armed === true, mode: t.mode, autopilot: t.autopilot } : null,
};

/** The canonical topic for a requested name (`mavlink.attitude` → `attitude`). */
export function canonicalTopic(topic: string): string | null {
  const bare = topic.startsWith("mavlink.") ? topic.slice("mavlink.".length) : topic;
  const key = bare === "HEARTBEAT" ? "heartbeat" : bare;
  return Object.hasOwn(TOPICS, key) ? key : null;
}

export function projectTopic(topic: string, t: VehicleState | null, ts: number): unknown | null {
  const key = canonicalTopic(topic);
  return key && t ? TOPICS[key](t, ts) : null;
}

function toPerceptionBatch(b: CockpitDetectionBatch): Record<string, unknown> {
  return {
    modelId: b.modelId,
    cameraId: b.cameraId,
    frameId: b.frameId,
    tsMs: b.tsMs,
    frameWidth: b.frameWidth,
    frameHeight: b.frameHeight,
    detections: b.detections.map((d) => ({
      bbox: d.bbox ? { x: d.bbox.x, y: d.bbox.y, width: d.bbox.width, height: d.bbox.height } : undefined,
      classLabel: d.classLabel,
      confidence: d.confidence,
      trackId: d.trackId ?? null,
      lockState: d.lockState ?? null,
    })),
  };
}

/** Topic bus shared by every extension frame in this cockpit. */
const busListeners = new Set<(topic: string, payload: unknown, from: string) => void>();

const HEALTH_WINDOW_MS = 2000;

export interface PluginHost {
  handle: (env: RpcEnvelope) => void;
  dispose: () => void;
}

export function createPluginHost(ext: InstalledExtension, post: (env: RpcEnvelope) => void): PluginHost {
  const telemetrySubs = new Set<string>();
  const eventSubs = new Set<string>();
  let perceptionSub = false;
  const batchTimes: number[] = [];

  const event = (method: string, args: unknown) =>
    post({ type: "event", method, capability: "", args, version: PROTOCOL_VERSION });

  const offFlight = useFlightStore.subscribe((s, prev) => {
    if (s.telemetry === prev.telemetry || telemetrySubs.size === 0) return;
    const ts = Date.now();
    for (const topic of telemetrySubs) {
      const frame = projectTopic(topic, s.telemetry, ts);
      if (frame !== null) event(`telemetry.${topic}`, frame);
    }
  });

  const offDetections = useDetectionsStore.subscribe((s, prev) => {
    if (s.latest === prev.latest || !s.latest) return;
    batchTimes.push(s.latest.receivedAt);
    while (batchTimes.length && s.latest.receivedAt - batchTimes[0] > HEALTH_WINDOW_MS) batchTimes.shift();
    if (perceptionSub) event("perception.detections", toPerceptionBatch(s.latest));
  });

  const onBus = (topic: string, payload: unknown, from: string) => {
    if (from !== ext.pluginId && eventSubs.has(topic)) event(topic, payload);
  };
  busListeners.add(onBus);

  const perceptionHealth = () => {
    const latest = useDetectionsStore.getState().latest;
    const now = Date.now();
    const recent = batchTimes.filter((t) => now - t <= HEALTH_WINDOW_MS);
    const fresh = latest != null && now - latest.receivedAt <= HEALTH_WINDOW_MS;
    return {
      session: fresh ? "live" : latest ? "stalled" : "idle",
      feed: fresh ? "fresh" : "stale",
      ageMs: latest ? now - latest.receivedAt : null,
      batchesPerSecond: recent.length / (HEALTH_WINDOW_MS / 1000),
      boundNode: null,
    };
  };

  const handlers: Record<string, (args: unknown) => Promise<unknown> | unknown> = {
    ping: () => ({ pong: Date.now() }),
    "i18n.t": (args) => (args as { key?: unknown } | null)?.key ?? "",
    "telemetry.subscribe": (args) => {
      const topic = (args as { topic: string }).topic;
      if (!canonicalTopic(topic)) throw Object.assign(new Error(`unsupported topic ${topic}`), { code: "unsupported_topic" });
      telemetrySubs.add(topic);
      const frame = projectTopic(topic, useFlightStore.getState().telemetry, Date.now());
      if (frame !== null) event(`telemetry.${topic}`, frame);
      return {};
    },
    "telemetry.unsubscribe": (args) => {
      const topic = (args as { topic?: unknown } | null)?.topic;
      if (typeof topic === "string") telemetrySubs.delete(topic);
      return {};
    },
    "perception.subscribe": () => {
      perceptionSub = true;
      return {};
    },
    "perception.unsubscribe": () => {
      perceptionSub = false;
      return {};
    },
    "perception.read": () => {
      const latest = useDetectionsStore.getState().latest;
      return latest ? toPerceptionBatch(latest) : { detections: [] };
    },
    "perception.health": perceptionHealth,
    "events.subscribe": (args) => {
      eventSubs.add((args as { topic: string }).topic);
      return {};
    },
    "events.unsubscribe": (args) => {
      eventSubs.delete((args as { topic: string }).topic);
      return {};
    },
    "events.publish": (args) => {
      const { topic, payload } = args as { topic: string; payload?: unknown };
      for (const l of busListeners) l(topic, payload, ext.pluginId);
      return {};
    },
    "command.send": async (args) => {
      const a = (args ?? {}) as { command?: unknown; args?: unknown };
      if (a.command !== PLUGIN_CONFIG_WRITE_COMMAND) {
        throw Object.assign(new Error(`command ${String(a.command)} is not offered by this cockpit`), {
          code: "unsupported_on_platform",
        });
      }
      const body = (a.args ?? {}) as { key?: unknown; value?: unknown };
      if (typeof body.key !== "string") throw Object.assign(new Error("config key required"), { code: "schema_invalid" });
      await apiFetch(`/api/plugins/${encodeURIComponent(ext.pluginId)}/config`, {
        method: "PUT",
        body: { key: body.key, value: body.value },
      });
      return {};
    },
  };

  return {
    handle(env) {
      if (env.type !== "request" || typeof env.method !== "string" || env.id === undefined) return;
      const method = env.method;
      const respond = (args: unknown, error?: { code: string; message: string }) =>
        post({ id: env.id, type: "response", method, capability: env.capability ?? "", args: args ?? null, version: PROTOCOL_VERSION, error });

      const decision = authorizeCall(method, env.args, ext.grantedCapabilities);
      if (!decision.ok) {
        respond(null, { code: decision.code, message: decision.message });
        return;
      }
      const handler = Object.hasOwn(handlers, method) ? handlers[method] : undefined;
      if (!handler) {
        respond(null, { code: "unsupported_on_platform", message: `${method} is not offered by this cockpit` });
        return;
      }
      Promise.resolve()
        .then(() => handler(env.args))
        .then(
          (result) => respond(result),
          (err: unknown) =>
            respond(null, {
              code: (err as { code?: string }).code ?? "handler_error",
              message: err instanceof Error ? err.message : String(err),
            }),
        );
    },
    dispose() {
      offFlight();
      offDetections();
      busListeners.delete(onBus);
      telemetrySubs.clear();
      eventSubs.clear();
    },
  };
}
