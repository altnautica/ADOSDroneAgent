import { describe, expect, it } from "vitest";

import { ApiError } from "../api-fetch";
import { fcSetupUrls, loadErrorText } from "./client";
import {
  cardState,
  cardSummary,
  cardTarget,
  firmwareLabel,
  isCancelled,
  overviewPrimary,
  readinessFlags,
  resultRows,
  runningStep,
  snapshotTime,
  wizardCards,
  wizardProgress,
} from "./overview";
import { parseInput, parseStatus, parseWizard } from "./parse";
import { motorOrderMarkup, setupErrorText, setupLabel, setupText } from "./strings";
import type { SetupStep, StepState, Wizard, WizardState } from "./types";

function step(id: string, state: StepState, extra: Partial<SetupStep> = {}): SetupStep {
  return {
    wizard: "ground",
    step: id,
    title_key: `setup.${id}.title`,
    body_key: `setup.${id}.body`,
    args: {},
    inputs: [],
    params: [],
    image: null,
    state,
    progress: null,
    live: {},
    actions: [],
    blockers: [],
    result: null,
    ...extra,
  };
}

function wizard(state: WizardState, steps: SetupStep[]): Wizard {
  return { id: "ground", state, available: true, blockers: [], completed_at: null, steps };
}

describe("interrupted wizard", () => {
  it("parses the interrupted state from the wizard and the status body", () => {
    expect(parseWizard({ id: "ground", state: "interrupted", steps: [] })?.state).toBe("interrupted");
    const status = parseStatus({ uid: "fc", wizards: [{ id: "ground", state: "interrupted", steps: [] }] });
    expect(status?.wizards[0].state).toBe("interrupted");
    expect(parseWizard({ id: "ground", state: "paused", steps: [] })).toBeNull();
  });

  it("builds the resume route", () => {
    expect(fcSetupUrls.resume("ground")).toBe("/api/v1/fc-setup/ground/resume");
  });
});

describe("overview primary button", () => {
  it("reads Continue for an interrupted or running wizard, Start before any run, Run again after", () => {
    const label = (s: WizardState) => setupText(overviewPrimary(s).label_key);
    expect(overviewPrimary("interrupted").kind).toBe("resume");
    expect(label("interrupted")).toBe("Continue setup");
    expect(overviewPrimary("running").kind).toBe("continue");
    expect(label("running")).toBe("Continue setup");
    expect(overviewPrimary("not_started").kind).toBe("start");
    expect(label("not_started")).toBe("Start setup");
    expect(label("done")).toBe("Run again");
    expect(overviewPrimary("aborted").kind).toBe("run_again");
  });
});

describe("component cards", () => {
  it("derives each card state from its steps", () => {
    expect(cardState([step("accel", "done"), step("level", "done"), step("compass", "skipped")])).toBe("done");
    expect(cardState([step("accel", "done"), step("level", "failed")])).toBe("attention");
    expect(cardState([step("frame", "waiting_user", { blockers: ["setup.blocker.armed"] })])).toBe("attention");
    expect(cardState([step("frame", "pending")])).toBe("not_set");
    expect(cardState([step("ados_intro", "pending")], true)).toBe("optional");
    expect(cardState([step("notch_static", "skipped")])).toBe("not_needed");
    expect(cardState([step("accel", "done"), step("level", "waiting_user")])).toBe("in_progress");
    // A blocker on a finished step no longer needs attention.
    expect(cardState([step("frame", "done", { blockers: ["setup.blocker.armed"] })])).toBe("done");
  });

  it("groups the ground wizard by component, hides empty cards and the detect step", () => {
    const w = wizard("running", [
      step("detect", "done"),
      step("frame", "done"),
      step("accel", "done"),
      step("level", "waiting_user"),
      step("compass", "pending"),
      step("radio", "pending"),
      step("outputs", "pending"),
      step("battery", "failed"),
      step("ados_profile", "pending"),
      step("ados_intro", "pending"),
    ]);
    const cards = wizardCards(w);
    expect(cards.map((c) => c.id)).toEqual(["frame", "sensors", "radio", "outputs", "battery", "safety", "ados"]);
    const byId = Object.fromEntries(cards.map((c) => [c.id, c]));
    expect(byId.sensors.steps.map((s) => s.step)).toEqual(["accel", "level", "compass"]);
    expect(byId.frame.state).toBe("done");
    expect(byId.sensors.state).toBe("in_progress");
    expect(byId.battery.state).toBe("attention");
    expect(byId.radio.state).toBe("not_set");
    expect(byId.ados.state).toBe("optional");
    expect(setupText(byId.outputs.title_key)).toBe("Motors & outputs");
    expect(wizardProgress(w)).toEqual({ done: 2, total: 9 });
  });

  it("gives any other wizard one card per step", () => {
    const air = { ...wizard("not_started", [step("preflight", "pending"), step("autotune", "pending")]), id: "air" };
    expect(wizardCards(air).map((c) => c.id)).toEqual(["preflight", "autotune"]);
  });

  it("summarises a card from its stored result", () => {
    const [frame] = wizardCards(
      wizard("done", [step("frame", "done", { result: { frame_class: 1, frame_type: 1, prop_in: 10, cells: 4 } })]),
    );
    expect(cardSummary(frame)).toBe("Quad · X · 10 in props");
    const [notch] = wizardCards(wizard("done", [step("initial_params", "skipped"), step("notch_static", "skipped")]));
    expect(cardSummary(notch)).toBe("Not needed");
  });
});

describe("frame picture picker", () => {
  it("reads the per-option pictures of an enum input", () => {
    const input = parseInput("frame_class", {
      kind: "enum",
      value: 1,
      options: [1, 2, 3],
      label_key: "setup.input.frame_class",
      images: { "1": "motor_order_quad_x", "2": "motor_order_hexa_x", "3": 7 },
    });
    expect(input?.images).toEqual({ "1": "motor_order_quad_x", "2": "motor_order_hexa_x" });
    expect(parseInput("cells", { kind: "number", value: 4, label_key: "setup.input.cells" })?.images).toEqual({});
  });

  it("marks the motor under test on the motor-order picture", () => {
    const svg =
      '<svg><circle cx="10.0" cy="20.0" r="13"/><text x="10.0" y="20.0" fill="#EDEDED" stroke="none">1</text>' +
      '<circle cx="30.0" cy="40.0" r="13"/><text x="30.0" y="40.0" fill="#EDEDED" stroke="none">2</text></svg>';
    const sources = { "../cockpit-spec/setup-images/motor_order_quad_x.svg": svg };
    const out = motorOrderMarkup("motor_order_quad_x", 2, sources)!;
    expect(out).toContain('<circle cx="30.0" cy="40.0" r="15" style="fill:hsl(var(--warn));stroke:hsl(var(--warn))"/>');
    expect(out).toContain('<circle cx="10.0" cy="20.0" r="13"/>');
    expect(motorOrderMarkup("motor_order_quad_x", 9, sources)).toBeNull();
    expect(motorOrderMarkup("accel_level", 1, sources)).toBeNull();
    // Every shipped motor-order picture can mark each of its motors.
    const layouts: Array<[string, number]> = [
      ["motor_order_quad_x", 4], ["motor_order_quad_plus", 4], ["motor_order_quad_h", 4],
      ["motor_order_hexa_x", 6], ["motor_order_hexa_plus", 6], ["motor_order_octa_x", 8], ["motor_order_octa_plus", 8],
    ];
    for (const [key, motors] of layouts) {
      for (let n = 1; n <= motors; n += 1) expect(motorOrderMarkup(key, n), `${key} motor ${n}`).toContain("hsl(var(--warn))");
    }
  });
});

describe("setup errors", () => {
  it("reads a 503 status read as setup not available on this node", () => {
    const err = new ApiError("503 ADOS pilot not reachable", 503, { detail: "ADOS pilot not reachable" });
    expect(loadErrorText(err)).toBe(setupText("setup.blocker.setup_unavailable"));
    expect(loadErrorText(new ApiError("503", 503, { ok: false, err: "setup_unavailable" }))).toBe(
      setupText("setup.blocker.setup_unavailable"),
    );
    expect(loadErrorText(new Error("network down"))).toBe("network down");
  });

  it("words an engine refusal code from setup.err, then setup.blocker", () => {
    expect(setupErrorText("armed")).toBe("Disarm first");
    expect(setupErrorText("ground_not_done")).toBe("Complete the aircraft setup first");
    expect(setupErrorText("no_such_code")).toBe("no_such_code");
  });
});

describe("card states from the flight controller's truth", () => {
  const ground = [
    step("detect", "pending"),
    step("frame", "pending"),
    step("accel", "pending"),
    step("battery", "pending"),
    step("ados_profile", "pending"),
  ];

  it("counts a wizard filed as done as done, step records or not", () => {
    const w = wizard("done", ground);
    expect(wizardProgress(w)).toEqual({ done: 4, total: 4 });
    expect(wizardCards(w).every((c) => c.state === "done")).toBe(true);
    // A step with a state of its own keeps it.
    const failed = wizard("done", [step("frame", "done"), step("battery", "failed")]);
    expect(wizardCards(failed).find((c) => c.id === "battery")?.state).toBe("attention");
  });

  it("flags the card a failing readiness item's fix points at", () => {
    const flags = readinessFlags([
      {
        id: "fc_profile",
        ok: false,
        severity: "block",
        msg: "FC parameters differ: MAV_GCS_SYSID 255 (want 1)",
        params: [{ name: "MAV_GCS_SYSID", have: 255, want: "1" }],
        fix: { kind: "apply_profile" },
      },
      { id: "setup_needed", ok: false, severity: "block", msg: "Aircraft setup not completed", fix: { kind: "setup_step", step: "frame" } },
      { id: "rangefinder", ok: false, severity: "warn", msg: "No rangefinder", fix: { kind: "settings", section: "sensors" } },
      { id: "battery_failsafe", ok: true, severity: "block", msg: "ok", fix: { kind: "setup_step", step: "battery" } },
    ]);
    expect([...flags.keys()]).toEqual(["ados_profile", "frame"]);
    const byId = Object.fromEntries(wizardCards(wizard("done", ground), flags).map((c) => [c.id, c]));
    expect(byId.safety.state).toBe("attention");
    expect(cardSummary(byId.safety)).toBe("MAV_GCS_SYSID 255 → 1");
    expect(byId.frame.state).toBe("attention");
    expect(cardSummary(byId.frame)).toBe("Aircraft setup not completed");
    expect(byId.battery.state).toBe("done");
    // The card opens the flagged step first.
    const sensors = wizardCards(wizard("not_started", [step("accel", "done"), step("compass", "pending")]), new Map([["accel", "x"]]))[0];
    expect(cardTarget(sensors, new Map([["accel", "x"]])).step).toBe("accel");
    // A run in progress on the flagged card stays in progress.
    const running = wizardCards(wizard("running", [step("ados_profile", "waiting_user")]), flags)[0];
    expect(running.state).toBe("in_progress");
  });

  it("reads an operator-cancelled step as neutral, not needing attention", () => {
    const cancelled = step("frame", "failed", { blockers: ["setup.blocker.cancelled"] });
    expect(isCancelled(cancelled)).toBe(true);
    expect(cardState([cancelled])).toBe("not_set");
    expect(isCancelled(step("frame", "failed", { blockers: ["setup.blocker.timeout"] }))).toBe(false);
    expect(setupText("setup.state.cancelled")).toBe("Cancelled");
  });

  it("says After setup on flight-tuning cards that wait on the aircraft setup", () => {
    const air: Wizard = {
      ...wizard("not_started", [step("preflight", "pending"), step("autotune", "pending")]),
      id: "air",
      available: false,
      blockers: ["setup.blocker.ground_not_done"],
    };
    expect(wizardCards(air).map((c) => c.state)).toEqual(["after_setup", "after_setup"]);
    expect(setupText("setup.card.state.after_setup")).toBe("After setup");
    expect(wizardCards({ ...air, available: true, blockers: [] }).map((c) => c.state)).toEqual(["not_set", "not_set"]);
  });
});

describe("a run in progress outside the wizard's own state", () => {
  it("reads Continue whenever a run is in progress, whatever the filed state", () => {
    expect(overviewPrimary("done", true).kind).toBe("continue");
    expect(overviewPrimary("aborted", true).kind).toBe("continue");
    expect(overviewPrimary("done").kind).toBe("run_again");
  });

  it("takes the running step from the status's top-level running", () => {
    const w = wizard("done", [step("ados_profile", "waiting_user")]);
    const status = { uid: "fc", firmware: "ardupilot", firmware_version: null, vehicle_class: null, wizards: [w], report: null };
    expect(runningStep({ ...status, running: { wizard: "ground", step: "ados_profile" } }, w)).toBe("ados_profile");
    expect(runningStep({ ...status, running: { wizard: "air", step: "autotune" } }, w)).toBeNull();
    expect(runningStep({ ...status, running: null }, w)).toBeNull();
  });
});

describe("setup copy", () => {
  it("names firmware as its project writes it", () => {
    expect(firmwareLabel("ardupilot")).toBe("ArduPilot");
    expect(firmwareLabel("px4")).toBe("PX4");
    expect(firmwareLabel("Betaflight")).toBe("Betaflight");
    expect(firmwareLabel("betaflight")).toBe("Betaflight");
    expect(firmwareLabel(null)).toBeNull();
  });

  it("shows plain result rows: internal keys left out, firmware named", () => {
    const rows = resultRows({
      firmware: "ardupilot",
      firmware_version: "4.8.0",
      vehicle_class: "multirotor",
      uid: "abc",
      uid_source: "uid2",
      params: 1433,
      mission_state_inferred: false,
    });
    expect(rows.map(([k]) => k)).toEqual(["firmware", "firmware_version", "vehicle_class", "params"]);
    expect(rows[0][1]).toBe("ArduPilot");
    expect(setupLabel("result", "params")).toBe("Parameters read");
    expect(resultRows(null)).toEqual([]);
  });

  it("reads a snapshot name as a date and time", () => {
    const at = snapshotTime("2026-11-24T13-09-07Z.param");
    expect(at).toBe(new Date("2026-11-24T13:09:07Z").toLocaleString());
    expect(snapshotTime("2026-11-24T13:09:07Z")).toBe(at);
    expect(snapshotTime("setup.param")).toBeNull();
  });
});
