import { describe, expect, it } from "vitest";

import layout from "../cockpit-spec/layout.json";
import type { BatteryHealthInput } from "./battery";
import { alertSource, baseAlerts, pillAlerts, pillSummary, STORAGE_CRIT_PCT, STORAGE_WARN_PCT, type BaseAlertInputs } from "./status-pill";

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
    expect(a.find((x) => x.source === "fc_prearm")!.text).toBe("Flight controller: Throttle below failsafe");
    expect(a.find((x) => x.source === "storage")!.severity).toBe("critical");
  });

  it("grades storage at the spec's 90 and 97 percent", () => {
    expect([STORAGE_WARN_PCT, STORAGE_CRIT_PCT]).toEqual([layout.status_pill.storage_warn_pct, layout.status_pill.storage_crit_pct]);
    const storage = (pct: number) => pillAlerts({ ...none, storagePct: pct }).find((x) => x.source === "storage")?.severity ?? "ok";
    expect([89, 90, 96, 97].map(storage)).toEqual(["ok", "warning", "warning", "critical"]);
    expect(pillAlerts({ ...none, storagePct: 90 })[0].text).toBe("Storage 90%");
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
});
