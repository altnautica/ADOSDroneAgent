import { describe, expect, it } from "vitest";

import layout from "../cockpit-spec/layout.json";
import type { BatteryHealthInput } from "./battery";
import alertGroups from "../cockpit-spec/alert-groups.json";
import type { ReadinessItem } from "./cockpit-frames";
import pillCases from "./fixtures/pill-cases.json";
import {
  ALERT_GROUPS,
  alertGroup,
  alertSource,
  baseAlerts,
  controllerLost,
  failsafeText,
  groupAlerts,
  pilotFailsafe,
  pillAlerts,
  pillSummary,
  radioFleetApplyOf,
  repairingMisKeyed,
  STORAGE_CRIT_PCT,
  STORAGE_WARN_PCT,
  unsnoozedAlerts,
  type BaseAlertInputs,
  type PairRearm,
  type PillAlert,
  type PillInputs,
} from "./status-pill";

function inputs(over: Partial<BaseAlertInputs> = {}): BaseAlertInputs {
  return { status: null, telemetry: null, battery: null, msSinceLive: 0, video: "live", ...over };
}

const fc = (remaining: number | null, voltage: number | null, armed = false): NonNullable<BaseAlertInputs["telemetry"]> => ({
  armed,
  battery: { voltage, remaining },
});

function engine(remaining: number | null, over: Partial<BatteryHealthInput> = {}): BatteryHealthInput {
  return { enabled: true, stale: false, packs: [{ id: 0, stale: false, remaining_pct: remaining, prediction: { eta_s: 240 } }], ...over };
}

const none = { alerts: [], readiness: [], prearm: [] as string[], storagePct: 40 };
const lowBattery = { id: "batt_low", level: "warning" as const, title: "Battery low" };
const linkLost = { id: "link_lost", level: "critical" as const, title: "Vehicle link lost" };

describe("base alerts: battery", () => {
  it("never fires on an unknown battery", () => {
    expect(baseAlerts(inputs({ telemetry: fc(0, 0) }))).toEqual([]);
    expect(baseAlerts(inputs({ telemetry: fc(-1, 16.2) }))).toEqual([]);
    expect(baseAlerts(inputs({ telemetry: fc(null, 16.2) }))).toEqual([]);
  });

  it("alerts on a measured low or critical pack with the spec's ids and texts", () => {
    expect(baseAlerts(inputs({ telemetry: fc(18, 15.1) }))).toEqual([
      { id: "batt_low", level: "warning", title: "Battery low", detail: "18% remaining" },
    ]);
    expect(baseAlerts(inputs({ battery: engine(8) }))).toEqual([
      { id: "batt_crit", level: "critical", title: "Battery critical", detail: "8% remaining" },
    ]);
  });

  it("raises an open battery-engine anomaly per pack and rule", () => {
    const battery = engine(80, {
      packs: [
        {
          id: 1,
          stale: false,
          remaining_pct: 80,
          anomalies: [
            { rule: "cell_imbalance", severity: "warning" },
            { rule: "old", severity: "critical", cleared_at_ms: 5 },
            { rule: "note", severity: "info" },
          ],
        },
      ],
    });
    expect(baseAlerts(inputs({ battery }))).toEqual([{ id: "battery_pack_1_cell_imbalance", level: "warning", title: "Battery 2: cell imbalance" }]);
  });
});

describe("base alerts: link, GNSS, video, uplink", () => {
  it("is critical only when lost while armed", () => {
    expect(baseAlerts(inputs({ telemetry: fc(null, null, true), msSinceLive: 12_000 }))[0]).toEqual({
      id: "link_lost",
      level: "critical",
      title: "Vehicle link lost",
      detail: "No telemetry for 12 s while armed",
    });
    expect(baseAlerts(inputs({ telemetry: fc(null, null), msSinceLive: 12_000 }))[0]).toMatchObject({
      id: "link_stale",
      level: "warning",
      detail: "Last telemetry 12 s ago",
    });
  });

  it("uses the arm state at the last live sample, since the snapshot drops `armed`", () => {
    expect(baseAlerts(inputs({ telemetry: {}, msSinceLive: 12_000, armedAtLastLive: true }))[0]).toMatchObject({ id: "link_lost" });
    expect(baseAlerts(inputs({ telemetry: {}, msSinceLive: 12_000, armedAtLastLive: false }))[0]).toMatchObject({ id: "link_stale" });
  });

  it("stays quiet when the vehicle was never live", () => {
    expect(baseAlerts(inputs({ msSinceLive: null }))).toEqual([]);
  });

  it("puts critical before warning before advisory, ids as Mission Control's", () => {
    const alerts = baseAlerts(
      inputs({
        telemetry: { ...fc(5, 14, true), gps: { fix_type: 2 } },
        video: "frozen",
        status: { network: { uplink_type: "wifi", uplink_reachable: false } },
      }),
    );
    expect(alerts.map((a) => [a.id, a.level])).toEqual([
      ["batt_crit", "critical"],
      ["gps_no_3d", "warning"],
      ["video_frozen", "warning"],
      ["uplink", "advisory"],
    ]);
    expect(alerts.find((a) => a.id === "gps_no_3d")!.detail).toBe("2D fix");
    expect(alerts.find((a) => a.id === "uplink")!.detail).toBe("wifi is not reachable");
  });

  it("files each alert under its pill source", () => {
    expect(["link_lost", "link_stale", "batt_crit", "batt_low", "battery_pack_0_x", "gps_no_3d", "video_frozen", "rf"].map(alertSource)).toEqual([
      "link",
      "link",
      "battery",
      "battery",
      "battery",
      "nav_source",
      "video",
      "link",
    ]);
  });

  it("warns while the ground station re-pairs a drone on the air with a different key", () => {
    const link = (pair_rearm: PairRearm | null) => ({ link: { pair_rearm } });
    expect(baseAlerts(inputs({ status: link({ cause: "mis_keyed", active: true, at: 700, episodes: 1 }) }))).toEqual([
      { id: "gs_repairing", level: "warning", title: "Ground station is re-pairing: its drone is on the air with a different key." },
    ]);
    // Another cause, a closed window, or no re-arm at all: nothing to say.
    expect(baseAlerts(inputs({ status: link({ cause: "unproven", active: true }) }))).toEqual([]);
    expect(baseAlerts(inputs({ status: link({ cause: "mis_keyed", active: false }) }))).toEqual([]);
    expect(baseAlerts(inputs({ status: link(null) }))).toEqual([]);
    expect(repairingMisKeyed(undefined)).toBe(false);
    expect(alertSource("gs_repairing")).toBe("link");
  });

  it("says a delivered fleet slot waits for landing, and when the restart onto it fails", () => {
    const fleet = (outcome: string, fleetSlot: number | null = 3) => ({ radioFleetApply: { outcome, fleetId: 7, fleetSlot } });
    const pending = baseAlerts(inputs({ status: fleet("deferred_armed") }));
    expect(pending).toEqual([{ id: "fleet_slot_pending", level: "advisory", title: "New fleet slot 3 applies after landing." }]);
    // An advisory: never in the pill.
    expect(pillAlerts({ ...none, alerts: pending })).toEqual([]);
    expect(baseAlerts(inputs({ status: fleet("restart_failed") }))).toEqual([
      { id: "fleet_slot_failed", level: "warning", title: "Radio restart onto fleet slot 3 failed. Retrying." },
    ]);
    // Applied, not yet judged, or no slot to name: nothing to say.
    expect(baseAlerts(inputs({ status: fleet("applied") }))).toEqual([]);
    expect(baseAlerts(inputs({ status: fleet("pending") }))).toEqual([]);
    expect(baseAlerts(inputs({ status: fleet("deferred_armed", null) }))).toEqual([]);
    expect(alertSource("fleet_slot_failed")).toBe("link");
  });

  it("reads the full status's fleet-apply block, typed fields only", () => {
    expect(radioFleetApplyOf({ outcome: "deferred_armed", fleetId: 7, fleetSlot: 3, unit: "ados-wfb-drone" })).toEqual({
      outcome: "deferred_armed",
      fleetId: 7,
      fleetSlot: 3,
    });
    expect(radioFleetApplyOf({ outcome: 1, fleetSlot: "3" })).toEqual({ outcome: null, fleetId: null, fleetSlot: null });
    expect(radioFleetApplyOf(null)).toBeNull();
    expect(radioFleetApplyOf([])).toBeNull();
  });
});

describe("status pill", () => {
  it("reads All systems OK in the ok colour with nothing active", () => {
    const s = pillSummary(pillAlerts(none), new Set());
    expect(s).toMatchObject({ severity: "ok", text: "All systems OK", count: 0, flashing: false });
  });

  it("shows the one alert's text, or the count, coloured by the worst", () => {
    const one = pillSummary(pillAlerts({ ...none, alerts: [lowBattery] }), new Set());
    expect(one).toMatchObject({ severity: "warning", text: "Battery low", count: 1, flashing: true });
    const many = pillSummary(
      pillAlerts({ ...none, alerts: [lowBattery, linkLost], prearm: ["PreArm: Compass not calibrated"] }),
      new Set(),
    );
    expect(many).toMatchObject({ severity: "critical", text: "3 warnings", count: 3 });
    expect(many.alerts[0].source).toBe("link");
    expect(pillSummary(many.alerts, new Set(many.alerts.map((a) => a.id))).flashing).toBe(false);
  });

  it("collects pilot readiness, FC pre-arm and storage, never an advisory", () => {
    const a = pillAlerts({
      alerts: [{ id: "uplink", level: "advisory", title: "Uplink offline" }],
      readiness: [
        { id: "gps", ok: false, severity: "block", msg: "No GNSS fix" },
        { id: "ekf", ok: true, severity: "block", msg: "" },
      ],
      prearm: ["PreArm: Throttle below failsafe"],
      storagePct: 98,
    });
    expect(a.map((x) => x.source).sort()).toEqual(["fc_prearm", "pilot_readiness", "storage"]);
    expect(a.find((x) => x.source === "fc_prearm")!.text).toBe("Throttle below failsafe");
    expect(a.find((x) => x.source === "storage")!.severity).toBe("critical");
  });

  it("grades storage at the spec's 90 and 97 percent", () => {
    expect([STORAGE_WARN_PCT, STORAGE_CRIT_PCT]).toEqual([layout.status_pill.storage_warn_pct, layout.status_pill.storage_crit_pct]);
    const storage = (pct: number) => pillAlerts({ ...none, storagePct: pct }).find((x) => x.source === "storage")?.severity ?? "ok";
    expect([89, 90, 96, 97].map(storage)).toEqual(["ok", "warning", "warning", "critical"]);
    expect(pillAlerts({ ...none, storagePct: 90 })[0].text).toBe("90 % used");
  });

  it("raises FC warnings from the pilot kind, critical at ERROR or worse, skipping PreArm lines", () => {
    const a = pillAlerts({
      ...none,
      fcStatus: [
        { severity: 2, text: "Crash: Disarming", ageMs: 0 },
        { severity: 4, text: "PreArm: Compass not calibrated", ageMs: 100 },
        { severity: 4, text: "EKF3 lane switch 1", ageMs: 900 },
      ],
    });
    expect(a.map((x) => [x.source, x.severity, x.text])).toEqual([
      ["fc_status_text", "critical", "Crash: Disarming"],
      ["fc_status_text", "warning", "EKF3 lane switch 1"],
    ]);
  });

  it("counts a repeated FC warning once, at the worst severity it was sent with", () => {
    const a = pillAlerts({
      ...none,
      fcStatus: [
        { severity: 4, text: "Vibration compensation ON", ageMs: 0 },
        { severity: 4, text: "Vibration compensation ON", ageMs: 2000 },
        { severity: 2, text: "Vibration compensation ON", ageMs: 4000 },
      ],
    });
    expect(a.map((x) => [x.id, x.severity])).toEqual([["fcst:Vibration compensation ON", "critical"]]);
    expect(pillSummary(a, new Set()).count).toBe(1);
  });

  it("shows a blocking readiness item as critical and a warning item as a warning", () => {
    const a = pillAlerts({
      ...none,
      readiness: [
        { id: "gps", ok: false, severity: "block", msg: "No GNSS fix" },
        { id: "no_agl", ok: false, severity: "warn", msg: "No height source" },
      ],
    });
    expect(a.map((x) => [x.id, x.severity])).toEqual([
      ["pilot:gps", "critical"],
      ["pilot:no_agl", "warning"],
    ]);
  });

  it("never reads OK without a fresh pilot state, and drops what the stale frame said", () => {
    const block = { id: "gps", ok: false, severity: "block" as const, msg: "No GNSS fix" };
    const fcStatus = [{ severity: 4, text: "EKF3 lane switch 1", ageMs: 0 }];
    const ids = (pilotAgeMs: number | null | undefined) =>
      pillAlerts({ ...none, readiness: [block], fcStatus, controllerLost: true, pilotAgeMs }).map((a) => a.id);
    expect(ids(undefined)).toEqual(["pilot:gps", "controller_lost", "fcst:EKF3 lane switch 1"]);
    expect(ids(1999)).toEqual(["pilot:gps", "controller_lost", "fcst:EKF3 lane switch 1"]);
    expect(ids(2000)).toEqual(["pilot_not_answering"]);
    expect(ids(null)).toEqual(["pilot_not_answering"]);
    const s = pillSummary(pillAlerts({ ...none, pilotAgeMs: null }), new Set());
    expect(s).toMatchObject({ severity: "warning", text: "Pilot not answering" });
    expect(s.alerts[0].source).toBe("pilot");
  });

  it("raises the agent, controller and pre-flight items from the spec", () => {
    const a = pillAlerts({
      ...none,
      pilotAgeMs: 0,
      agentReachable: false,
      controllerLost: true,
      preflightFailing: [{ id: "gps", label: "GNSS fix" }],
    });
    expect(a.map((x) => [x.id, x.source, x.severity, x.text])).toEqual([
      ["agent_unreachable", "link", "critical", "Agent unreachable"],
      ["controller_lost", "controller", "warning", "Controller lost"],
      ["preflight:gps", "preflight", "warning", "GNSS fix"],
    ]);
  });

  it("calls the controller lost only while armed and airborne", () => {
    const p = { holdReason: "controller-lost", armed: true, landed: false };
    expect(controllerLost(p)).toBe(true);
    expect(controllerLost({ ...p, landed: true })).toBe(false);
    expect(controllerLost({ ...p, armed: false })).toBe(false);
    expect(controllerLost({ ...p, holdReason: "link-loss" })).toBe(false);
    expect(controllerLost(null)).toBe(false);
    // An unknown arm state is possibly airborne.
    expect(controllerLost({ ...p, armed: null, landed: null })).toBe(true);
  });
});

describe("pill golden cases (fixtures/pill-cases.json)", () => {
  for (const c of pillCases) {
    it(c.name, () => {
      const i = c.input;
      const alerts = pillAlerts({
        alerts: i.alerts as { id: string; level: "critical" | "warning" | "advisory"; title: string }[],
        readiness: i.readiness as unknown as ReadinessItem[],
        prearm: i.prearm ?? undefined,
        storagePct: i.storage_pct,
        fcStatus: i.fc_status.map((s) => ({ severity: s.severity, text: s.text, ageMs: s.age_ms })),
        pilotAgeMs: i.pilot_age_ms,
        agentReachable: i.agent_reachable,
        controllerLost: i.controller_lost,
        preflightFailing: i.preflight_failing,
        fcFailsafe: i.fc_failsafe,
        failsafeCause: i.failsafe_cause,
        setupAbort: i.setup_abort,
        lowBatteryAction: i.low_battery_action,
        armed: i.armed,
        navSource: i.nav_source as PillInputs["navSource"],
      });
      const s = pillSummary(alerts, new Set());
      const e = c.expected as typeof c.expected & { detail?: unknown };
      const { detail, ...face } = e;
      expect({
        severity: s.severity,
        text: s.text,
        count: s.count,
        ids: alerts.map((a) => a.id),
        groups: groupAlerts(alerts, new Set()).map((g) => g.group),
      }).toEqual(face);
      if (detail) {
        expect(alerts.map((a) => ({ id: a.id, group: a.group, key: a.key, params: a.params, fix: a.fix }))).toEqual(detail);
      }
    });
  }
});

describe("failsafe item", () => {
  it("takes its inputs from the pilot state and words an unknown cause as itself", () => {
    expect(pilotFailsafe(null)).toEqual({ fcFailsafe: false, failsafeCause: null, lowBatteryAction: null });
    expect(pilotFailsafe({ fcFailsafe: true, failsafeCause: "rc_or_failsafe", lowBatteryAction: null, rthCountdownS: null })).toEqual({
      fcFailsafe: true,
      failsafeCause: "rc_or_failsafe",
      lowBatteryAction: null,
    });
    // The low-battery item waits for the countdown, which has its own banner.
    expect(pilotFailsafe({ fcFailsafe: false, failsafeCause: null, lowBatteryAction: "land", rthCountdownS: 4 }).lowBatteryAction).toBe(null);
    expect(pilotFailsafe({ fcFailsafe: false, failsafeCause: null, lowBatteryAction: "land", rthCountdownS: null }).lowBatteryAction).toBe("land");
    expect(failsafeText("battery")).toBe("Failsafe: battery");
  });

  it("shows only from a fresh pilot state", () => {
    expect(pillAlerts({ ...none, fcFailsafe: true }).map((a) => a.id)).toEqual(["failsafe"]);
    expect(pillAlerts({ ...none, pilotAgeMs: null, fcFailsafe: true }).map((a) => a.id)).toEqual(["pilot_not_answering"]);
  });
});

describe("alert groups", () => {
  const fcProfile: ReadinessItem = {
    id: "fc_profile",
    ok: false,
    severity: "block",
    msg: "FC parameters differ",
    params: [{ name: "MAV_GCS_SYSID", have: 255, want: "1" }],
    fix: { kind: "apply_profile" },
  };
  const fcTexts = [
    { severity: 4, text: "EKF variance", ageMs: 100 },
    { severity: 4, text: "Compass interference", ageMs: 200 },
  ];

  it("the spec orders every group, and each section title comes from it", () => {
    expect([...alertGroups.order].sort()).toEqual([...ALERT_GROUPS].sort());
    for (const g of ALERT_GROUPS) expect(typeof alertGroups.titles[g]).toBe("string");
  });

  it("maps readiness ids, alert ids and sources, and unmapped ids to other", () => {
    expect(alertGroup("pilot:fc_profile", "pilot_readiness")).toBe("setup");
    expect(alertGroup("pilot:flow", "pilot_readiness")).toBe("sensors");
    expect(alertGroup("pilot:something_new", "pilot_readiness")).toBe("other");
    expect(alertGroup("fcst:EKF variance", "fc_status_text")).toBe("flight_controller");
    expect(alertGroup("alert:batt_low", "battery")).toBe("power");
    expect(alertGroup("alert:gps_no_3d", "nav_source")).toBe("navigation");
    expect(alertGroup("storage", "storage")).toBe("system");
    expect(alertGroup("setup_abort", "pilot")).toBe("setup");
  });

  it("groups each pre-flight check with the alerts of its kind", () => {
    const checks = ["link", "gps", "home", "battery", "something_new"].map((id) => ({ id, label: id }));
    const alerts = pillAlerts({ ...none, armed: false, preflightFailing: checks });
    expect(Object.fromEntries(alerts.map((a) => [a.id, a.group]))).toEqual({
      "preflight:link": "link",
      "preflight:gps": "navigation",
      "preflight:home": "navigation",
      "preflight:battery": "power",
      "preflight:something_new": "other",
    });
  });

  it("an fc_profile critical and two FC texts group as setup, then flight controller", () => {
    const alerts = pillAlerts({ ...none, readiness: [fcProfile], fcStatus: fcTexts });
    const sections = groupAlerts(alerts, new Set());
    expect(sections.map((s) => [s.group, s.title, s.alerts.length, s.hidden])).toEqual([
      ["setup", "Flight controller setup", 1, 0],
      ["flight_controller", "Flight controller messages", 2, 0],
    ]);
    expect(sections[0].alerts[0]).toMatchObject({ params: fcProfile.params, fix: { kind: "apply_profile" }, key: "pilot:fc_profile|FC parameters differ" });
  });

  it("orders critical before warning inside a group, then as raised", () => {
    const alerts = pillAlerts({
      ...none,
      fcStatus: [{ severity: 4, text: "A", ageMs: 0 }, { severity: 2, text: "B", ageMs: 0 }, { severity: 4, text: "C", ageMs: 0 }],
    });
    expect(groupAlerts(alerts, new Set())[0].alerts.map((a) => a.text)).toEqual(["B", "A", "C"]);
  });

  it("folds identical texts in a group into one row at the worst severity", () => {
    const a: PillAlert[] = [
      { id: "x", source: "fc_status_text", severity: "warning", text: "Same", group: "flight_controller", key: "x|Same" },
      { id: "y", source: "fc_prearm", severity: "critical", text: "Same", group: "flight_controller", key: "y|Same" },
    ];
    const [s] = groupAlerts(a, new Set());
    expect(s.alerts.map((r) => [r.id, r.severity])).toEqual([["x", "critical"]]);
  });

  it("a snoozed warning moves to hidden; a snoozed critical stays, collapsed", () => {
    const alerts = pillAlerts({ ...none, readiness: [fcProfile], fcStatus: fcTexts });
    const warn = alerts.find((a) => a.text === "EKF variance");
    const crit = alerts.find((a) => a.id === "pilot:fc_profile");
    const snoozed = new Set([warn!.key, crit!.key]);
    const sections = groupAlerts(alerts, snoozed);
    expect(sections[0].alerts).toEqual([{ ...crit, collapsed: true }]);
    expect(sections[1].alerts.map((a) => a.text)).toEqual(["Compass interference"]);
    expect(sections[1].hidden).toBe(1);
    // The pill counts the snoozed critical and drops the snoozed warning.
    expect(pillSummary(unsnoozedAlerts(alerts, snoozed), new Set()).count).toBe(2);
  });

  it("a section whose every alert is hidden stays, empty, with its hidden count", () => {
    const alerts = pillAlerts({ ...none, fcStatus: [fcTexts[0]] });
    expect(groupAlerts(alerts, new Set([alerts[0].key]))).toEqual([
      { group: "flight_controller", title: "Flight controller messages", alerts: [], hidden: 1 },
    ]);
  });
});
