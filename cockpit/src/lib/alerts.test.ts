import { BatteryLow, BatteryWarning, Satellite, SignalZero, VideoOff, WifiOff } from "lucide-react";
import { describe, expect, it } from "vitest";

import { computeAlerts, pillWiring, type AlertInputs } from "@/lib/alerts";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { pillAlerts } from "@/shared/hud/status-pill";

function inputs(over: Partial<AlertInputs> = {}): AlertInputs {
  return { status: null, telemetry: null, battery: null, msSinceLive: 0, video: "live", ...over };
}

describe("cockpit alerts", () => {
  it("carry the shared rules' ids, each with its icon, worst first", () => {
    const alerts = computeAlerts(
      inputs({
        telemetry: { armed: true, battery: { voltage: 14, current: null, remaining: 5, temperature: null }, gps: { fix_type: 2, satellites: 5, eph: null, epv: null } },
        video: "frozen",
        status: { network: { uplink_type: "wifi", uplink_reachable: false } } as AlertInputs["status"],
      }),
    );
    expect(alerts.map((a) => [a.id, a.level, a.icon])).toEqual([
      ["batt_crit", "critical", BatteryWarning],
      ["gps_no_3d", "warning", Satellite],
      ["video_frozen", "warning", VideoOff],
      ["uplink", "advisory", WifiOff],
    ]);
    expect(computeAlerts(inputs({ telemetry: { armed: true }, msSinceLive: 12_000 }))[0]).toMatchObject({ id: "link_lost", icon: SignalZero });
    expect(
      computeAlerts(inputs({ telemetry: { battery: { voltage: 15, current: null, remaining: 18, temperature: null } } }))[0],
    ).toMatchObject({ id: "batt_low", icon: BatteryLow });
  });
});

describe("the status pill's pilot, agent, controller and pre-flight inputs", () => {
  const pilot = (over: Partial<PilotState> = {}) => ({ at: 1000, data: { holdReason: null, armed: false, landed: true, ...over } as PilotState });
  const checks = [
    { id: "link", label: "Vehicle link fresh", fail: "Vehicle link not fresh", ok: false },
    { id: "gps", label: "GPS fix 3D or better", fail: "GPS fix below 3D", ok: false },
    { id: "home", label: "Home position set", fail: "Home position not set", ok: false },
    { id: "battery", label: "Battery above the low band", fail: "Battery in the low band", ok: true },
  ];
  const base = { pilot: pilot(), nowMs: 1500, statusStale: false, armed: false, checks, alerts: [] };

  it("passes the pilot frame's age, null with no pilot state", () => {
    expect(pillWiring(base).pilotAgeMs).toBe(500);
    expect(pillWiring({ ...base, pilot: null }).pilotAgeMs).toBeNull();
  });

  it("never reads OK with no pilot state, and drops a stale frame's readiness", () => {
    const readiness = [{ id: "position", ok: false, severity: "block" as const, msg: "No position estimate" }];
    const none = pillAlerts({ alerts: [], readiness, prearm: undefined, storagePct: null, ...pillWiring({ ...base, pilot: null, checks: [] }) });
    expect(none.map((a) => a.id)).toEqual(["pilot_not_answering"]);
    const stale = pillAlerts({ alerts: [], readiness, prearm: undefined, storagePct: null, ...pillWiring({ ...base, nowMs: 3000, checks: [] }) });
    expect(stale.map((a) => a.id)).toEqual(["pilot_not_answering"]);
  });

  it("raises the FC failsafe from a fresh pilot state only", () => {
    const failsafe = { ...base, pilot: pilot({ fcFailsafe: true, failsafeCause: "fc" }), checks: [] };
    const fresh = pillAlerts({ alerts: [], readiness: [], prearm: undefined, storagePct: null, ...pillWiring(failsafe) });
    expect(fresh.map((a) => [a.id, a.severity, a.text])).toEqual([["failsafe", "critical", "Failsafe: flight controller"]]);
    const stale = pillAlerts({ alerts: [], readiness: [], prearm: undefined, storagePct: null, ...pillWiring({ ...failsafe, nowMs: 3000 }) });
    expect(stale.map((a) => a.id)).toEqual(["pilot_not_answering"]);
  });

  it("calls the agent unreachable while its status poll fails", () => {
    expect(pillWiring(base).agentReachable).toBe(true);
    expect(pillWiring({ ...base, statusStale: true }).agentReachable).toBe(false);
  });

  it("raises a lost controller only in the air", () => {
    expect(pillWiring({ ...base, pilot: pilot({ holdReason: "controller-lost", armed: true, landed: false }) }).controllerLost).toBe(true);
    expect(pillWiring({ ...base, pilot: pilot({ holdReason: "controller-lost" }) }).controllerLost).toBe(false);
  });

  it("lists failing pre-flight items on the ground only, never twice", () => {
    expect(pillWiring(base).preflightFailing.map((c) => c.id)).toEqual(["link", "gps", "home"]);
    const covered = pillWiring({ ...base, alerts: [{ id: "link_lost", level: "critical", title: "Link lost" }, { id: "gps_no_3d", level: "warning", title: "No 3D fix" }] });
    expect(covered.preflightFailing.map((c) => c.id)).toEqual(["home"]);
    expect(pillWiring({ ...base, armed: true }).preflightFailing).toEqual([]);
  });

  it("words each failing pre-flight item as the failure, not the passing label", () => {
    const covered = pillWiring({ ...base, alerts: [{ id: "link_lost", level: "critical", title: "Link lost" }] });
    expect(covered.preflightFailing).toEqual([
      { id: "gps", label: "GPS fix below 3D" },
      { id: "home", label: "Home position not set" },
    ]);
  });

  it("lists no pre-flight items while the arm state is unknown (FC link down)", () => {
    expect(pillWiring({ ...base, armed: undefined }).preflightFailing).toEqual([]);
  });
});
