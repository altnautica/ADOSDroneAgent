import { readFileSync } from "node:fs";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { vehicleLiveness } from "@/hooks/use-vehicle-liveness";
import {
  airframeResetAlertId,
  batteryFixAlertId,
  FOLLOW_PLUGIN_ID,
  freshNavFrames,
  homeFollow,
  homeNavSource,
  homePill,
  homeStatusLines,
  pillArmed,
  poseAgeText,
  sendFollowAction,
  type HomeStatusInputs,
} from "@/lib/home-status";
import { specText } from "@/shared/hud/spec-text";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import promptCases from "@/shared/hud/fixtures/follow-prompt-cases.json";
import storageCases from "@/shared/hud/fixtures/storage-cases.json";
import { parseFollowState } from "@/shared/hud/nav-types";
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

  it("says why the setup wizard aborted", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [], setup_abort: "setup.blocker.aborted_battery" });
    const pill = homePill({ ...base, pilot });
    expect(pill.alerts.map((a) => [a.id, a.severity, a.text])).toEqual([["setup_abort", "warning", "Aborted: battery below 30 %"]]);
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
    expect(freshNavFrames(body, 10_000, 10_500)).toEqual({ pilot: null, follow: null, nav: null });
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
    expect(lost.alerts[0]).toMatchObject({ id: "alert:link_lost", source: "link", severity: "critical" });    // The arm state is unknown with the FC link down: no pre-flight items.
    expect(pill.alerts.filter((a) => a.source === "preflight")).toEqual([]);
    expect(lost.alerts.filter((a) => a.source === "preflight")).toEqual([]);
  });

  it("stays OK while the vehicle stamp keeps moving", () => {
    let l = INITIAL_LIVENESS;
    for (let t = 0; t <= 5_000; t += 1_000) l = nextLiveness(l, { ...groundTel, last_update: String(t) }, t);
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    expect(homePill({ ...base, pilot, telemetry: groundTel, ...vehicleLiveness(l, 5_000, true) }).text).toBe("All systems OK");
  });

  it("lists a failing pre-flight check on the ground, and none in the air", () => {
    const pilot = parsePilotState({ ...raw, armed: false, readiness: [], statustext: [] });
    const noHome = { ...groundTel, home: null };
    const pill = homePill({ ...base, pilot, telemetry: noHome });
    expect(pill.alerts.map((a) => [a.id, a.source, a.text])).toEqual([["preflight:home", "preflight", "Home position not set"]]);
    const airborne = parsePilotState({ ...raw, armed: true, readiness: [], statustext: [] });
    expect(homePill({ ...base, pilot: airborne, telemetry: { ...noHome, armed: true } }).text).toBe("All systems OK");
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

  // The cockpit runs the same golden frames (`cockpit/src/lib/storage-pill.test.ts`):
  // a drone reports storage as `health.disk_percent`, a ground station as
  // `system.disk_pct`, and both surfaces list the same items.
  for (const c of storageCases) {
    it(`storage item: ${c.name}`, () => {
      const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
      const pill = homePill({ ...base, pilot, status: c.input.full });
      expect(pill.alerts.map((a) => ({ source: a.source, severity: a.severity }))).toEqual(c.expected);
    });
  }

  it("hides the pre-flight items while the arm state is unknown", () => {
    const noHome = { ...groundTel, home: null };
    const unknown = parsePilotState({ ...raw, armed: null, readiness: [], statustext: [] });
    expect(unknown?.armed).toBeNull();
    const pill = homePill({ ...base, pilot: unknown, telemetry: noHome });
    expect(pill.alerts.filter((a) => a.source === "preflight")).toEqual([]);
    // Without a pilot frame and no arm state in telemetry it is unknown too.
    const noArm = { ...noHome, armed: undefined };
    expect(pillArmed({ pilot: null, telemetry: noArm })).toBeNull();
    expect(pillArmed({ pilot: null, telemetry: noHome })).toBe(false);
  });

  it("judges the arm state from the fresh pilot frame and live telemetry only", () => {
    const noHome = { ...groundTel, home: null };
    // The pilot process stalled on a disarmed frame; the FC armed from RC.
    const staleDisarmed = parsePilotState({ ...raw, armed: false, readiness: [], statustext: [] });
    const pill = homePill({
      ...base,
      pilot: staleDisarmed,
      pilotAgeMs: 5_000,
      telemetry: { ...noHome, armed: true },
      armSources: { pilot: null, telemetry: { ...noHome, armed: true } },
    });
    expect(pill.alerts.filter((a) => a.source === "preflight")).toEqual([]);
    // Neither source current: unknown, so the pre-flight items stay hidden.
    expect(pillArmed({ pilot: null, telemetry: null })).toBeNull();
    const unknown = homePill({ ...base, pilot: staleDisarmed, telemetry: noHome, armSources: { pilot: null, telemetry: null } });
    expect(unknown.alerts.filter((a) => a.source === "preflight")).toEqual([]);
  });

  it("raises the nav-source item from a live EKF report that has lost position", () => {
    const pilot = parsePilotState({ ...raw, readiness: [], statustext: [] });
    const pill = homePill({ ...base, pilot, telemetry: { ...groundTel, ekf: { flags: 0, age_ms: 100 } } });
    expect(pill.alerts.map((a) => [a.id, a.source])).toContainEqual(["nav_source_degraded", "nav_source"]);
    expect(homeNavSource({ telemetry: groundTel, live: true, nav: null, pilot: null }).kind).toBe("stale");
  });

  it("turns the nav source crit while the pilot reports an unverified position", () => {
    const pilot = parsePilotState({ ...raw, position_unverified: { reason: "flow_disagrees" } });
    const tel = { ...groundTel, ekf: { flags: 2 + 8, age_ms: 100 } };
    expect(homeNavSource({ telemetry: tel, live: true, nav: null, pilot })).toMatchObject({ tone: "crit", quality: "flowDisagrees" });
    expect(homeNavSource({ telemetry: tel, live: true, nav: null, pilot: null }).tone).not.toBe("crit");
  });

  it("reads the pose age from the fresh nav frame", () => {
    const body = { nav: { age_ms: 0, data: { mode: "vio", pose_age_ms: 42.4 } } };
    const nav = freshNavFrames(body, 1_000, 1_000).nav;
    expect(poseAgeText(nav)).toBe("Pose age 42 ms");
    expect(poseAgeText(null)).toBeNull();
  });
});

describe("the battery failsafe fix on the pill", () => {
  const pilotWith = (key: string, armed: boolean) =>
    parsePilotState({ ...raw, armed, readiness: [{ id: "battery_failsafe", ok: false, severity: "warn", msg: specText(key) }] });

  it("lists the item and marks its alert for the fix while disarmed", () => {
    const pilot = pilotWith("readiness.battery_failsafe.critical_none", false);
    const pill = homePill({ ...base, pilot });
    expect(pill.alerts.map((a) => a.id)).toContain("pilot:battery_failsafe");
    expect(batteryFixAlertId({ pilot, telemetry: groundTel })).toBe("pilot:battery_failsafe");
  });

  it("offers no fix while armed or on unset levels", () => {
    expect(batteryFixAlertId({ pilot: pilotWith("readiness.battery_failsafe.none", true), telemetry: groundTel })).toBeNull();
    expect(batteryFixAlertId({ pilot: pilotWith("readiness.battery_failsafe.levels_unset", false), telemetry: groundTel })).toBeNull();
    expect(batteryFixAlertId({ pilot: null, telemetry: groundTel })).toBeNull();
  });

  it("links the fix to Aircraft setup and runs the ADOS profile step", () => {
    const src = readFileSync(path.resolve(__dirname, "../components/panels/status-tiles.tsx"), "utf8");
    expect(src).toContain('to="/settings/aircraft-setup"');
    expect(src).toContain("rerunStep(BATTERY_FAILSAFE_FIX_STEP.wizard, BATTERY_FAILSAFE_FIX_STEP.step)");
  });
});

describe("home follow line", () => {
  const follow = (raw: Record<string, unknown>) => homeFollow(parseFollowState(raw));

  it("shows the waiting prompt title when follow holds for the operator", () => {
    const f = follow({
      active: true,
      phase: "holding-await-operator",
      hold_reason: "search-exhausted",
      reacquire_mode: "smart",
      track_id: 7,
      operator_resume_seq: 4,
    });
    expect(f).toEqual({
      word: "Waiting for you #7",
      tone: "crit",
      prompt: "Target lost: holding, waiting for instructions",
      body: "The search found no match for the subject.",
      actions: ["resume", "stop"],
      resumeSeq: 4,
      height: null,
      overhead: null,
    });
  });

  it("adds the seconds left of the search while recovering", () => {
    for (const [phase, word] of [
      ["predicting", "Predicting #3 9.5 s"],
      ["repositioning", "Moving to look #3 9.5 s"],
      ["searching", "Searching #3 9.5 s"],
    ] as const) {
      const f = follow({ active: true, phase, track_id: 3, reacquire_remaining_s: 9.46, search_step: 1 });
      expect(f).toMatchObject({ word, tone: "coast", prompt: null, body: null, actions: [] });
    }
    expect(follow({ active: true, phase: "following", track_id: 3, reacquire_remaining_s: 9.46 })?.word).toBe("Following #3");
  });

  it("has no line while follow is off or unheard", () => {
    expect(homeFollow(null)).toBeNull();
    expect(follow({ active: false })).toBeNull();
  });

  it("shows the height lowered under leaves and the pilot's overhead clearance ahead while each applies", () => {
    const under = { active: true, phase: "following", height_setpoint_m: 3.87, height_lowered: true, height_reason: "canopy_floor" };
    const pilot = parsePilotState({ ...raw, overhead_ahead: { lease_id: 7, fly_under_agl_m: 3.1, ahead_m: 12.4 } });
    expect(homeFollow(parseFollowState(under), pilot)).toMatchObject({
      height: { text: "Flying under leaves: 3.9 m", reason: "At the lowest follow height, leaves lower still" },
      overhead: "Overhead clearance ahead: 3.1 m in 12 m",
    });
    const clear = parsePilotState({ ...raw, overhead_ahead: { lease_id: 7, fly_under_agl_m: null, ahead_m: null } });
    expect(follow({ active: true, phase: "following", height_setpoint_m: 6 })).toMatchObject({ height: null, overhead: null });
    expect(homeFollow(parseFollowState(under), clear)?.overhead).toBeNull();
  });

  for (const c of promptCases) {
    it(`shared prompt case: ${c.name}`, () => {
      const f = follow(c.state);
      expect(f?.prompt ?? null).toBe(c.expect ? "Target lost: holding, waiting for instructions" : null);
      // Read-mostly: of the prompt's actions only resume and stop are offered here.
      const want = (c.expect?.actions ?? []).filter((a) => a === "resume" || a === "stop");
      expect(f?.actions ?? []).toEqual(want);
    });
  }

  it("reads the fresh follow frame with its recovery fields", () => {
    const body = { follow: { age_ms: 0, data: { active: true, phase: "searching", reacquire_remaining_s: 4, track_id: 2 } } };
    expect(homeFollow(freshNavFrames(body, 1_000, 1_000).follow)?.word).toBe("Searching #2 4.0 s");
  });
});

describe("follow actions from the Home card", () => {
  const fetchMock = vi.fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>();

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });
  afterEach(() => vi.unstubAllGlobals());

  it("resume writes the next resume sequence and stop turns follow off, through the plugin config", async () => {
    fetchMock.mockImplementation(async () => new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "Content-Type": "application/json" } }));
    await sendFollowAction("resume", 4);
    await sendFollowAction("stop", 4);
    const url = `/api/plugins/${FOLLOW_PLUGIN_ID}/config`;
    expect(FOLLOW_PLUGIN_ID).toBe("com.altnautica.follow-me");
    expect(fetchMock.mock.calls.map(([u, init]) => [u, init?.method, JSON.parse(String(init?.body))])).toEqual([
      [url, "PUT", { key: "operator_resume_seq", value: 5 }],
      [url, "PUT", { key: "active", value: false }],
    ]);
  });
});

describe("the steady take-off and Balance check rows on the pill", () => {
  const NOSE = "Balance: nose-heavy. Off by about 35 mm. Move the battery back.";
  const OFF = specText("readiness.supervised_liftoff.off");
  const pilotWith = (severity: "warn" | "block", armed: boolean) =>
    parsePilotState({
      ...raw,
      armed,
      readiness: [
        { id: "airframe_balance", ok: false, severity, msg: NOSE },
        { id: "supervised_liftoff", ok: false, severity: "warn", msg: OFF },
      ],
    });

  it("lists both rows with their words and level, and offers I rebalanced it while disarmed", () => {
    for (const [severity, level] of [
      ["warn", "warning"],
      ["block", "critical"],
    ] as const) {
      const pilot = pilotWith(severity, false);
      const alerts = homePill({ ...base, pilot }).alerts;
      expect(alerts.find((a) => a.id === "pilot:airframe_balance")).toMatchObject({ text: NOSE, severity: level });
      expect(alerts.find((a) => a.id === "pilot:supervised_liftoff")).toMatchObject({ text: OFF, severity: "warning" });
      expect(airframeResetAlertId({ pilot, telemetry: groundTel })).toBe("pilot:airframe_balance");
    }
  });

  it("offers no reset while armed or without a pilot", () => {
    expect(airframeResetAlertId({ pilot: pilotWith("warn", true), telemetry: groundTel })).toBeNull();
    expect(airframeResetAlertId({ pilot: null, telemetry: groundTel })).toBeNull();
  });
});
