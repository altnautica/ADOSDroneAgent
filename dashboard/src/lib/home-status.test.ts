import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { homePill, homeStatusLines, type HomeStatusInputs } from "@/lib/home-status";
import { parsePilotState } from "@/shared/hud/cockpit-frames";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const raw = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")) as Record<string, unknown>;

const base: HomeStatusInputs = { pilot: parsePilotState(raw), telemetry: null, battery: null, status: null, msSinceLive: 0, pilotAgeMs: 100, agentReachable: true };

describe("home status card", () => {
  it("lists the readiness, prearm, FC status text and storage alerts in the pill", () => {
    const pilot = parsePilotState({
      ...raw,
      readiness: [{ id: "fc_profile", ok: false, severity: "block", msg: "FC failsafe parameters do not match" }],
      statustext: [{ severity: 2, text: "EKF3 IMU0 yaw inconsistent", age_ms: 400 }],
    });
    const pill = homePill({
      ...base,
      pilot,
      telemetry: { prearm: ["PreArm: Compass not calibrated"] },
      status: { system: { disk_pct: 97 } },
    });
    const sources = pill.alerts.map((a) => a.source).sort();
    expect(sources).toEqual(["fc_prearm", "fc_status_text", "pilot_readiness", "storage"]);
    expect(pill.severity).toBe("critical");
    expect(pill.alerts.length).toBe(4);
  });

  it("reads all systems OK with nothing active", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    expect(homePill({ ...base, pilot }).text).toBe("All systems OK");
  });

  it("builds the link-loss, AGL, sticks and time-to-reserve lines from the shared rules", () => {
    const pilot = parsePilotState({ ...raw, link_loss_in_s: 3.4, link_loss_action: "rth", agl_m: 4.12, agl_source: "home" });
    const lines = homeStatusLines({
      ...base,
      pilot,
      follow: null,
      battery: { enabled: true, stale: false, packs: [{ id: 0, stale: false, remaining_pct: 72, prediction: { eta_s: 610 } }] },
    });
    expect(lines.linkLoss).toBe("Link lost: Return home in 4 s");
    expect(lines.agl).toEqual({ text: "4.1 m", mark: "HOME" });
    expect(lines.sticks).toMatch(/^Sticks are with /);
    expect(lines.timeToReserve).toBe("10:10 left");
  });

  it("warns once when the kept pilot frame is stale and drops the items it carried", () => {
    const pilot = parsePilotState({
      ...raw,
      readiness: [{ id: "fc_profile", ok: false, severity: "block", msg: "FC failsafe parameters do not match" }],
    });
    const pill = homePill({ ...base, pilot, pilotAgeMs: 5_000 });
    expect(pill.alerts.map((a) => a.id)).toEqual(["pilot_not_answering"]);
    expect(homePill({ ...base, pilot: null, pilotAgeMs: null }).alerts.map((a) => a.id)).toEqual(["pilot_not_answering"]);
  });

  it("raises a critical link item while the agent does not answer", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    const pill = homePill({ ...base, pilot, agentReachable: false });
    expect(pill.alerts.find((a) => a.id === "agent_unreachable")?.severity).toBe("critical");
    expect(pill.severity).toBe("critical");
  });

  it("flags a lost controller while armed and airborne", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [], hold_reason: "controller-lost", armed: true, landed: false });
    expect(homePill({ ...base, pilot }).alerts.map((a) => a.id)).toContain("controller_lost");
  });
});
