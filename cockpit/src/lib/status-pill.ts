// The top bar's status pill (cockpit spec `layout.json` `status_pill`): one
// summary of everything that should stop a pilot from flying carelessly.
// Sources: the cockpit's own alerts (link, battery, GNSS, frozen video, RF
// verdict), the pilot's readiness items, the flight controller's PreArm/Arm
// texts and on-board storage. It reads "All systems OK", the one active
// alert's text, or "<n> warnings", coloured by the worst severity. A new
// alert makes it flash until acknowledged; opening the list acknowledges.

import type { CockpitAlert } from "@/lib/alerts";
import { STORAGE_BLOCK_PCT, STORAGE_WARN_PCT } from "@/lib/cockpit-flow";
import { cockpitText } from "@/lib/cockpit-text";
import type { FcStatusText, ReadinessItem } from "@/shared/hud/cockpit-frames";

export type PillSource = "pilot_readiness" | "fc_prearm" | "fc_status_text" | "battery" | "link" | "nav_source" | "storage" | "video";
export type PillSeverity = "ok" | "warning" | "critical";

export interface PillAlert {
  id: string;
  source: PillSource;
  severity: "warning" | "critical";
  text: string;
}

export interface PillInputs {
  alerts: readonly CockpitAlert[];
  readiness: readonly ReadinessItem[];
  /** The FC's PreArm/Arm texts from the last 10 s; undefined while unknown. */
  prearm: readonly string[] | undefined;
  storagePct: number | null;
  /** The pilot kind's FC STATUSTEXT at WARNING or worse (last 30 s). */
  fcStatus?: readonly FcStatusText[];
}

function alertSource(id: string): PillSource {
  if (id.startsWith("battery")) return "battery";
  if (id === "gps") return "nav_source";
  if (id === "video") return "video";
  return "link";
}

/** Every active pill alert, worst first, then in source order. Advisories
 *  (an offline uplink) are not warnings and stay out of the pill. */
export function pillAlerts(i: PillInputs): PillAlert[] {
  const out: PillAlert[] = [];
  for (const a of i.alerts) {
    if (a.level === "advisory") continue;
    out.push({ id: `alert:${a.id}`, source: alertSource(a.id), severity: a.level, text: a.title });
  }
  for (const r of i.readiness) {
    if (r.ok) continue;
    // A blocking item refuses take-off: critical, as the pre-flight sheet fails it.
    out.push({ id: `pilot:${r.id}`, source: "pilot_readiness", severity: r.severity === "block" ? "critical" : "warning", text: r.msg || r.id });
  }
  if (i.prearm && i.prearm.length > 0) {
    const text = i.prearm[i.prearm.length - 1].replace(/^(PreArm|Arm):\s*/, "");
    out.push({ id: `fc:${text}`, source: "fc_prearm", severity: "warning", text: cockpitText("flow.preflight.fc_prearm.failing", { text }) });
  }
  // The FC repeats a warning while its condition lasts: one alert per text,
  // at the worst severity it was sent with.
  const fcAt = new Map<string, number>();
  for (const s of i.fcStatus ?? []) {
    // PreArm/Arm lines already reach the pill through `prearm`.
    if (/^(PreArm|Arm):/.test(s.text)) continue;
    const severity = s.severity <= 2 ? "critical" : "warning";
    const at = fcAt.get(s.text);
    if (at === undefined) {
      fcAt.set(s.text, out.length);
      out.push({ id: `fcst:${s.text}`, source: "fc_status_text", severity, text: s.text });
    } else if (severity === "critical") {
      out[at].severity = "critical";
    }
  }
  if (i.storagePct !== null && i.storagePct >= STORAGE_WARN_PCT) {
    out.push({
      id: "storage",
      source: "storage",
      severity: i.storagePct >= STORAGE_BLOCK_PCT ? "critical" : "warning",
      text: `${cockpitText("cockpit.status.source.storage")} ${Math.round(i.storagePct)}%`,
    });
  }
  return out.sort((a, b) => (a.severity === b.severity ? 0 : a.severity === "critical" ? -1 : 1));
}

export interface PillSummary {
  severity: PillSeverity;
  text: string;
  count: number;
  /** An alert not yet acknowledged is active. */
  flashing: boolean;
  alerts: readonly PillAlert[];
}

/** The pill's face for the active alerts and the acknowledged ids. */
export function pillSummary(alerts: readonly PillAlert[], acked: ReadonlySet<string>): PillSummary {
  const count = alerts.length;
  const severity: PillSeverity = count === 0 ? "ok" : alerts.some((a) => a.severity === "critical") ? "critical" : "warning";
  const text =
    count === 0
      ? cockpitText("cockpit.status.ok")
      : count === 1
        ? alerts[0].text || cockpitText("cockpit.status.one")
        : cockpitText("cockpit.status.many", { n: count });
  return { severity, text, count, flashing: alerts.some((a) => !acked.has(a.id)), alerts };
}
