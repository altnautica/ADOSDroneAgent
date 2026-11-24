import { describe, expect, it } from "vitest";

import { ApiError } from "../api-fetch";
import { fcSetupUrls, loadErrorText } from "./client";
import { cardState, cardSummary, overviewPrimary, wizardCards, wizardProgress } from "./overview";
import { parseInput, parseStatus, parseWizard } from "./parse";
import { motorOrderMarkup, setupErrorText, setupText } from "./strings";
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
