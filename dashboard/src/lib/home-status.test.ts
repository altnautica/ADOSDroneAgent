import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { vehicleLiveness } from "@/hooks/use-vehicle-liveness";
import { freshNavFrames, homePill, homeStatusLines, type HomeStatusInputs } from "@/lib/home-status";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import { INITIAL_LIVENESS, nextLiveness } from "@/shared/hud/vehicle-live";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const raw = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")) as Record<string, unknown>;

/** A healthy vehicle on the ground: live attitude, 3D fix, home set. */
const groundTel = {
  armed: false,
  attitude: { roll: 0, pitch: 0, yaw: 0 },
  gps: { fix_type: 3 },
  battery: { remaining: 80, voltage: 16.2 },
  home: { lat_e7: 129_700_000, lon_e7: 775_900_000 },
};

const base: HomeStatusInputs = {
  pilot: parsePilotState(raw),
  telemetry: groundTel,
  battery: null,
  status: null,
  live: true,
  msSinceLive: 0,
  armedAtLastLive: null,
  pilotAgeMs: 100,
  agentReachable: true,
};

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
      telemetry: { ...groundTel, prearm: ["PreArm: Compass not calibrated"] },
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
    expect(lines.sticks).toBe("Sticks are with Mission Control");
    expect(lines.timeToReserve).toBe("10:10 left");
  });

  it("hides the link-loss, AGL and sticks lines once the pilot frame is stale", () => {
    const pilot = { ...raw, link_loss_in_s: 3.4, link_loss_action: "rth", agl_m: 4.12, agl_source: "home" };
    const follow = { active: true, phase: "tracking", agl_m: 6.0, agl_source: "rng" };
    // Received at 10 s; the agent had kept the pilot frame 1.5 s already.
    const body = { pilot: { age_ms: 1_500, data: pilot }, follow: { age_ms: 1_500, data: follow } };
    const at = (now: number) => homeStatusLines({ ...base, ...freshNavFrames(body, 10_000, now) });
    const live = at(10_400);
    expect(live.linkLoss).toBe("Link lost: Return home in 4 s");
    expect(live.agl).toEqual({ text: "4.1 m", mark: "HOME" });
    expect(live.sticks).toBe("Sticks are with Mission Control");
    // 2 s after the frame left the pilot: stale, though the HTTP read is 0.5 s old.
    const stale = at(10_500);
    expect(stale).toMatchObject({ linkLoss: null, agl: null, sticks: null });
    expect(freshNavFrames(body, 10_000, 10_500)).toEqual({ pilot: null, follow: null });
    // A dead pilot: every poll answers at once, serving the kept frame with a growing age.
    const kept = { pilot: { age_ms: 2_100, data: pilot }, follow: { age_ms: 2_100, data: follow } };
    expect(homeStatusLines({ ...base, ...freshNavFrames(kept, 10_400, 10_500) })).toMatchObject({ linkLoss: null, agl: null, sticks: null });
  });

  it("lists a link item, never all systems OK, with the FC down and the agent answering", () => {
    // Live at 0 s and 1 s, armed; then the agent answers with the vehicle fields stripped.
    let l = nextLiveness(INITIAL_LIVENESS, { ...groundTel, armed: true, last_update: "a" }, 0);
    l = nextLiveness(l, { ...groundTel, armed: true, last_update: "b" }, 1_000);
    for (let t = 2_000; t <= 5_000; t += 1_000) l = nextLiveness(l, { armed: undefined }, t);
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    const fcDown = { ...base, pilot, telemetry: {}, ...vehicleLiveness(l, 5_000, true) };
    const pill = homePill(fcDown);
    expect(pill.text).not.toBe("All systems OK");
    expect(pill.alerts.find((a) => a.id === "alert:link_stale")?.source).toBe("link");
    // Past 10 s, armed at the last live sample: the critical link-lost item.
    for (let t = 6_000; t <= 12_000; t += 1_000) l = nextLiveness(l, { armed: undefined }, t);
    const lost = homePill({ ...fcDown, ...vehicleLiveness(l, 12_000, true) });
    expect(lost.alerts[0]).toMatchObject({ id: "alert:link_lost", source: "link", severity: "critical" });
  });

  it("stays OK while the vehicle stamp keeps moving", () => {
    let l = INITIAL_LIVENESS;
    for (let t = 0; t <= 5_000; t += 1_000) l = nextLiveness(l, { ...groundTel, last_update: String(t) }, t);
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    expect(homePill({ ...base, pilot, telemetry: groundTel, ...vehicleLiveness(l, 5_000, true) }).text).toBe("All systems OK");
  });

  it("lists a failing pre-flight check on the ground, and none in the air", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    const noHome = { ...groundTel, home: null };
    const pill = homePill({ ...base, pilot, telemetry: noHome });
    expect(pill.alerts.map((a) => [a.id, a.source, a.text])).toEqual([["preflight:home", "preflight", "Pre-flight: Home position set"]]);
    expect(homePill({ ...base, pilot, telemetry: { ...noHome, armed: true } }).text).toBe("All systems OK");
    // A check a listed base alert already reports is not listed twice.
    const noFix = homePill({ ...base, pilot, telemetry: { ...groundTel, gps: { fix_type: 1 } } });
    expect(noFix.alerts.map((a) => a.id)).toEqual(["preflight:gps"]);
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

  it("raises a critical failsafe item from a fresh pilot, and none from a stale one", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [], fc_failsafe: true, failsafe_cause: "rc_or_failsafe" });
    const pill = homePill({ ...base, pilot });
    expect(pill.severity).toBe("critical");
    expect(pill.text).toBe("Failsafe: RC switch or failsafe");
    expect(homePill({ ...base, pilot, pilotAgeMs: 5_000 }).alerts.map((a) => a.id)).toEqual(["pilot_not_answering"]);
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
