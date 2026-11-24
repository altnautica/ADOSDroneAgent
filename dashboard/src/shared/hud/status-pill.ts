// The top bar's status pill (cockpit spec `layout.json` `status_pill`): one
// summary of everything that should stop a pilot from flying carelessly.
// Sources: the surface's alerts (the base rules in `base-alerts.ts`, re-exported
// here, plus whatever the surface adds), the pilot's readiness items, the
// flight controller's PreArm/Arm and status texts, on-board storage, and the
// spec's pilot, link, controller and pre-flight rules. It reads "All systems
// OK", the one active alert's text, or "<n> warnings", coloured by the worst
// severity. A fresh pilot state reporting the FC's own failsafe RTL or land is
// critical. Every rule reads a real field; an unknown reading never alerts.

import alertGroupsJson from "../cockpit-spec/alert-groups.json";
import tokens from "../cockpit-spec/tokens.json";
import { STORAGE_CRIT_PCT, STORAGE_WARN_PCT, type AlertLevel } from "./base-alerts";
import type { FcStatusText, MissionStall, ParamDiff, PilotState, ReadinessFix, ReadinessItem } from "./cockpit-frames";
import { CHIP_STALE_AFTER_MS, shortTitle } from "./flight-state";
import { missionStallOverlay } from "./mission-stall";
import type { NavSourceKind, NavSourceResult } from "./nav-source";
import { hasSpecText, specText } from "./spec-text";

export {
  LINK_LOST_MS,
  LINK_STALE_MS,
  STORAGE_CRIT_PCT,
  STORAGE_WARN_PCT,
  baseAlerts,
  fleetSlotAlert,
  radioFleetApplyOf,
  repairingMisKeyed,
  sortAlerts,
  type AlertLevel,
  type BaseAlert,
  type BaseAlertInputs,
  type PairRearm,
  type RadioFleetApply,
} from "./base-alerts";

/** Every pill source, each worded by `cockpit.status.source.<source>`. */
export const PILL_SOURCES = [
  "pilot_readiness",
  "fc_prearm",
  "fc_status_text",
  "battery",
  "link",
  "nav_source",
  "storage",
  "video",
  "pilot",
  "controller",
  "preflight",
] as const;
export type PillSource = (typeof PILL_SOURCES)[number];
export type PillSeverity = "ok" | "warning" | "critical";

/** The colour of words in a severity: `text_critical` for critical, the
 *  warning hue (bright enough as text) for warning, plain text otherwise.
 *  `severity_critical` and the `state.*` hues are for dots, outlines and the
 *  lightbar only: as small text they fall below 4.5:1 over bright video. */
export function severityTextColor(s: PillSeverity): string {
  if (s === "critical") return tokens.chrome.text_critical;
  if (s === "warning") return tokens.chrome.severity_warning;
  return tokens.chrome.text;
}

export type { ParamDiff, ReadinessFix } from "./cockpit-frames";

/** The alert list's sections (cockpit spec `alert-groups.json` `order`). */
export const ALERT_GROUPS = ["setup", "flight_controller", "sensors", "navigation", "power", "link", "system", "other"] as const;
export type AlertGroup = (typeof ALERT_GROUPS)[number];

interface AlertGroupsSpec {
  readiness: Record<string, string>;
  id: Record<string, string>;
  source: Record<string, string>;
  order: string[];
  titles: Record<string, string>;
}
const GROUPS_SPEC: AlertGroupsSpec = alertGroupsJson;

function asGroup(g: string | undefined): AlertGroup {
  return (ALERT_GROUPS as readonly string[]).includes(g ?? "") ? (g as AlertGroup) : "other";
}

/** The section of an alert: a readiness item by its readiness id, else by
 *  the alert id, else by its source; anything unmapped is `other`. */
export function alertGroup(id: string, source: PillSource): AlertGroup {
  if (id.startsWith("pilot:")) return asGroup(GROUPS_SPEC.readiness[id.slice("pilot:".length)]);
  return asGroup(GROUPS_SPEC.id[id] ?? GROUPS_SPEC.source[source]);
}

/** A section's title (`alert-groups.json` `titles`). */
export function alertGroupTitle(g: AlertGroup): string {
  return GROUPS_SPEC.titles[g] ?? g;
}

export interface PillAlert {
  id: string;
  source: PillSource;
  severity: "warning" | "critical";
  text: string;
  group: AlertGroup;
  /** The snooze key: `id|text`, so the same id with new words shows again. */
  key: string;
  /** The parameter rows behind a readiness item. */
  params?: ParamDiff[];
  /** The fix a readiness item offers. */
  fix?: ReadinessFix;
}

type DraftAlert = Omit<PillAlert, "group" | "key">;

export interface PillInputs {
  alerts: readonly { id: string; level: AlertLevel; title: string }[];
  readiness: readonly ReadinessItem[];
  /** The FC's PreArm/Arm texts from the last 10 s; undefined while unknown. */
  prearm: readonly string[] | undefined;
  storagePct: number | null;
  /** The pilot kind's FC STATUSTEXT at WARNING or worse (last 30 s). */
  fcStatus?: readonly FcStatusText[];
  /** Age of the newest pilot state in ms; null when none ever arrived.
   *  Omitted (undefined) by a surface that does not read the pilot kind:
   *  then the pilot rule is off. With a number at or past
   *  `flight-state.json` `stale_after_ms`, or null, the pill shows one
   *  `pilot_not_answering` warning and drops the readiness, FC status text
   *  and controller items, which came from that stale frame, so it never
   *  reads OK without a fresh pilot state. */
  pilotAgeMs?: number | null;
  /** False while the agent itself does not answer: a critical link item. */
  agentReachable?: boolean;
  /** The pilot holds because the controller was lost, armed and airborne
   *  (see `controllerLost`). */
  controllerLost?: boolean;
  /** The failing pre-flight checklist items. The checklist is a ground
   *  surface: pass them only while on the ground. */
  preflightFailing?: readonly { id: string; label: string }[];
  /** The fresh pilot state reports the FC flying a failsafe RTL or land of
   *  its own (`fc_failsafe`): one critical `failsafe` item. Ignored while
   *  the pilot state is stale. */
  fcFailsafe?: boolean;
  /** Why (`failsafe_cause`: `fc`, `rc_or_failsafe`); null reads as `fc`. */
  failsafeCause?: string | null;
  /** The fresh pilot state's `setup_abort`: the setup-strings key of the
   *  air wizard's last abort that was not the operator's
   *  (`setup.blocker.aborted_<reason>`). One warning `setup_abort` item
   *  worded by that key (an unknown key reads as itself); ignored while the
   *  pilot state is stale. */
  setupAbort?: string | null;
  /** The pilot's own low-battery action once it fired
   *  (`low_battery_action` with no countdown running): `land` or `rth`,
   *  one critical `low_battery` item. Ignored while the pilot state is
   *  stale. */
  lowBatteryAction?: string | null;
  /** The arm state; null while unknown (the drone may be airborne): the
   *  pre-flight items are hidden. Omitted: not known to this surface. */
  armed?: boolean | null;
  /** The resolved nav source (`resolveNavSource`): a `warn` or `crit` tone
   *  raises one `nav_source_degraded` item. */
  navSource?: Pick<NavSourceResult, "kind" | "quality" | "tone"> | null;
  /** The fresh pilot state's `mission_stall`: one critical `mission_stall`
   *  item reading the stall overlay's title. Ignored while the pilot state
   *  is stale. */
  missionStall?: MissionStall | null;
}

/** The pill's failsafe inputs from a pilot state: the FC's own failsafe,
 *  and the pilot's low-battery action once its countdown has run out (the
 *  countdown has its own banner). */
export function pilotFailsafe(
  p: Pick<PilotState, "fcFailsafe" | "failsafeCause" | "lowBatteryAction" | "rthCountdownS"> | null,
): { fcFailsafe: boolean; failsafeCause: string | null; lowBatteryAction: string | null } {
  return {
    fcFailsafe: p?.fcFailsafe === true,
    failsafeCause: p?.failsafeCause ?? null,
    lowBatteryAction: p && p.rthCountdownS === null ? p.lowBatteryAction : null,
  };
}

/** The low-battery item's text for `action` (`land`, `rth`); null for an
 *  unknown action. The pilot announces the same line as a STATUSTEXT. */
export function lowBatteryText(action: string | null | undefined): string | null {
  if (action === "land") return specText("alert.low_battery_land");
  if (action === "rth") return specText("alert.low_battery_rth");
  return null;
}

/** `Failsafe: <cause>`, the cause worded by `pilot.banner.cause.*`; an
 *  unknown cause reads as itself. */
export function failsafeText(cause: string | null | undefined): string {
  const c = cause || "fc";
  const key = `pilot.banner.cause.${c}`;
  return specText("alert.failsafe", { cause: hasSpecText(key) ? specText(key) : c });
}

/** The pilot holds for a lost controller while armed and airborne (spec
 *  `status_pill.controller_rule`). An unknown arm state is possibly airborne. */
export function controllerLost(p: Pick<PilotState, "holdReason" | "armed" | "landed"> | null): boolean {
  return p !== null && p.holdReason === "controller-lost" && p.armed !== false && p.landed !== true;
}

/** The source word of a nav source kind (`nav_source.*`). */
const NAV_SOURCE_KEY: Readonly<Record<NavSourceKind, string | null>> = {
  gps: "nav_source.gps",
  ext: "nav_source.ext_nav",
  flow: "nav_source.flow",
  dr: "nav_source.dr",
  stale: null,
};

/** The pill source of an alert id. */
export function alertSource(id: string): PillSource {
  if (id === "batt_crit" || id === "batt_low" || id.startsWith("battery_pack_")) return "battery";
  if (id === "gps_no_3d") return "nav_source";
  if (id === "video_frozen") return "video";
  return "link";
}

/** Every active pill alert, worst first, then in source order. Advisories
 *  (an offline uplink) are not warnings and stay out of the pill. */
export function pillAlerts(i: PillInputs): PillAlert[] {
  const out: DraftAlert[] = [];
  for (const a of i.alerts) {
    if (a.level === "advisory") continue;
    out.push({ id: `alert:${a.id}`, source: alertSource(a.id), severity: a.level, text: a.title });
  }
  if (i.agentReachable === false) {
    out.push({ id: "agent_unreachable", source: "link", severity: "critical", text: specText("alert.agent_unreachable") });
  }
  const pilotStale = i.pilotAgeMs === null || (i.pilotAgeMs !== undefined && i.pilotAgeMs >= CHIP_STALE_AFTER_MS);
  if (pilotStale) {
    out.push({ id: "pilot_not_answering", source: "pilot", severity: "warning", text: specText("alert.pilot_not_answering") });
  } else if (i.fcFailsafe) {
    out.push({ id: "failsafe", source: "pilot", severity: "critical", text: failsafeText(i.failsafeCause) });
  }
  if (!pilotStale && i.setupAbort) {
    out.push({ id: "setup_abort", source: "pilot", severity: "warning", text: specText(i.setupAbort) });
  }
  const lowBattery = pilotStale ? null : lowBatteryText(i.lowBatteryAction);
  if (lowBattery) {
    out.push({ id: "low_battery", source: "battery", severity: "critical", text: lowBattery });
  }
  const stall = pilotStale ? null : missionStallOverlay(i.missionStall);
  if (stall) {
    out.push({ id: "mission_stall", source: "pilot", severity: stall.severity, text: stall.title });
  }
  for (const r of pilotStale ? [] : i.readiness) {
    if (r.ok) continue;
    // A blocking item refuses take-off: critical, as the pre-flight sheet fails it.
    const a: DraftAlert = { id: `pilot:${r.id}`, source: "pilot_readiness", severity: r.severity === "block" ? "critical" : "warning", text: r.msg || r.id };
    if (r.params && r.params.length > 0) a.params = r.params;
    if (r.fix) a.fix = r.fix;
    out.push(a);
  }
  if (i.controllerLost && !pilotStale) {
    out.push({ id: "controller_lost", source: "controller", severity: "warning", text: specText("alert.controller_lost") });
  }
  // Pre-flight items are a ground surface: hidden while the arm state is
  // unknown, since the drone may be airborne.
  for (const item of i.armed === null ? [] : (i.preflightFailing ?? [])) {
    out.push({ id: `preflight:${item.id}`, source: "preflight", severity: "warning", text: specText("alert.preflight_failing", { item: item.label }) });
  }
  const nav = i.navSource;
  const navKey = nav ? NAV_SOURCE_KEY[nav.kind] : null;
  if (nav && navKey && (nav.tone === "warn" || nav.tone === "crit")) {
    out.push({
      id: "nav_source_degraded",
      source: "nav_source",
      severity: nav.tone === "crit" ? "critical" : "warning",
      text: specText("alert.nav_source_degraded", { source: specText(navKey) }),
    });
  }
  // Every distinct PreArm/Arm line the FC repeats: each names its own cause.
  const prearmSeen = new Set<string>();
  for (const line of i.prearm ?? []) {
    const text = line.replace(/^(PreArm|Arm):\s*/, "");
    if (text === "" || prearmSeen.has(text)) continue;
    prearmSeen.add(text);
    out.push({ id: `fc:${text}`, source: "fc_prearm", severity: "warning", text });
  }
  // The FC repeats a warning while its condition lasts: one alert per text,
  // at the worst severity it was sent with. The pilot's own low-battery
  // announcement folds into its `low_battery` item.
  const fcAt = new Map<string, number>();
  if (lowBattery) fcAt.set(lowBattery, out.findIndex((a) => a.id === "low_battery"));
  for (const s of pilotStale ? [] : (i.fcStatus ?? [])) {
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
      severity: i.storagePct >= STORAGE_CRIT_PCT ? "critical" : "warning",
      text: specText("cockpit.status.storage_used", { pct: Math.round(i.storagePct) }),
    });
  }
  return out
    .sort((a, b) => (a.severity === b.severity ? 0 : a.severity === "critical" ? -1 : 1))
    .map((a) => ({ ...a, group: alertGroup(a.id, a.source), key: `${a.id}|${a.text}` }));
}

/** The alerts the pill counts: a snoozed warning drops out, a snoozed
 *  critical stays (it only folds in the list). */
export function unsnoozedAlerts(alerts: readonly PillAlert[], snoozed: ReadonlySet<string>): PillAlert[] {
  return alerts.filter((a) => a.severity === "critical" || !snoozed.has(a.key));
}

/** An alert in a section; `collapsed` marks a snoozed critical. */
export type GroupedAlert = PillAlert & { collapsed?: boolean };

export interface AlertSection {
  group: AlertGroup;
  title: string;
  alerts: GroupedAlert[];
  /** Snoozed warnings left out of `alerts`. */
  hidden: number;
}

/** The alert list's sections, in the spec's group order; inside one, critical
 *  before warning, then as raised. One row per text in a section (the worst
 *  severity wins). A snoozed warning is counted in `hidden`; a snoozed
 *  critical stays, `collapsed`. Empty sections are left out. */
export function groupAlerts(alerts: readonly PillAlert[], snoozed: ReadonlySet<string>): AlertSection[] {
  const byGroup = new Map<AlertGroup, GroupedAlert[]>();
  for (const a of alerts) {
    let g = byGroup.get(a.group);
    if (!g) {
      g = [];
      byGroup.set(a.group, g);
    }
    const same = g.find((b) => b.text === a.text);
    if (same) {
      if (a.severity === "critical") same.severity = "critical";
      continue;
    }
    g.push({ ...a });
  }
  const out: AlertSection[] = [];
  for (const name of GROUPS_SPEC.order) {
    const group = asGroup(name);
    const g = byGroup.get(group);
    if (!g) continue;
    const shown: GroupedAlert[] = [];
    let hidden = 0;
    for (const a of g) {
      if (!snoozed.has(a.key)) shown.push(a);
      else if (a.severity === "critical") shown.push({ ...a, collapsed: true });
      else hidden += 1;
    }
    // Stable: equal severities keep the order they were raised in.
    shown.sort((a, b) => (a.severity === b.severity ? 0 : a.severity === "critical" ? -1 : 1));
    out.push({ group, title: alertGroupTitle(group), alerts: shown, hidden });
  }
  return out;
}

export interface PillSummary {
  severity: PillSeverity;
  text: string;
  count: number;
  /** An alert not yet acknowledged is active. */
  flashing: boolean;
  alerts: readonly PillAlert[];
}

/** An alert's one-line title. With parameter rows behind it the table
 *  carries the detail, so the title is the plain words before the first
 *  colon ("FC parameters differ"); the full text stays the accessible name. */
export function alertTitle(a: Pick<PillAlert, "text" | "params">): string {
  return shortTitle(a.text, a.params);
}

/** The pill's face for the active alerts and the acknowledged ids. One
 *  alert reads its title; more read the count, the blocking ones first
 *  ("2 blocking · 5 warnings") so a warning count never hides a blocker. */
export function pillSummary(alerts: readonly PillAlert[], acked: ReadonlySet<string>): PillSummary {
  const count = alerts.length;
  const blocking = alerts.filter((a) => a.severity === "critical").length;
  const warnings = count - blocking;
  const severity: PillSeverity = count === 0 ? "ok" : blocking > 0 ? "critical" : "warning";
  const text =
    count === 0
      ? specText("cockpit.status.ok")
      : count === 1
        ? alertTitle(alerts[0]) || specText("cockpit.status.one")
        : blocking === 0
          ? specText("cockpit.status.many", { n: count })
          : warnings === 0
            ? specText("cockpit.status.blocking", { n: blocking })
            : warnings === 1
              ? specText("cockpit.status.blocking_one_warning", { n: blocking })
              : specText("cockpit.status.blocking_warnings", { n: blocking, m: warnings });
  return { severity, text, count, flashing: alerts.some((a) => !acked.has(a.id)), alerts };
}
