import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { SCREEN_TABLE } from "@/nav/screen-table";
import "@/shared/i18n";
import { setupText } from "@/shared/setup-wizard/strings";
import type { SetupStatus, SetupStep, StepState, Wizard } from "@/shared/setup-wizard/types";
import type { FcSetupController } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView, type WizardViewProps } from "@/shared/setup-wizard/wizard-view";

function controller(over: Partial<FcSetupController>): FcSetupController {
  return {
    status: null,
    loading: false,
    streamUp: true,
    loadError: null,
    requestError: null,
    busy: null,
    refresh: async () => undefined,
    start: async () => true,
    resume: async () => true,
    abort: async () => undefined,
    action: async () => undefined,
    rerun: async () => true,
    revert: async () => null,
    ...over,
  };
}

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

function status(ground: Partial<Wizard>): SetupStatus {
  return {
    uid: "fc-1",
    firmware: "ArduPilot",
    firmware_version: "4.7.3",
    vehicle_class: "multirotor",
    running: null,
    report: null,
    wizards: [{ id: "ground", state: "not_started", available: true, blockers: [], completed_at: null, steps: [], ...ground }],
  };
}

const render = (setup: FcSetupController) => renderToStaticMarkup(createElement(WizardView, { setup }));
const renderWith = (setup: FcSetupController, props: Partial<WizardViewProps>) =>
  renderToStaticMarkup(createElement(WizardView, { setup, ...props }));

describe("Aircraft setup screen", () => {
  it("shows the error, not the loading line, once a read failed with 503", () => {
    const unavailable = setupText("setup.blocker.setup_unavailable");
    const html = render(controller({ loading: false, loadError: unavailable }));
    expect(html).toContain('data-testid="setup-load-error"');
    expect(html).toContain("It is not available on this node.");
    expect(html).not.toContain("Reading the setup state");
  });

  it("shows the loading line only while the first read is in flight", () => {
    expect(render(controller({ loading: true }))).toContain("Reading the setup state");
  });

  it("offers Continue setup with the interrupted notice after an agent restart", () => {
    const html = render(
      controller({
        status: status({
          state: "interrupted",
          steps: [step("detect", "done"), step("frame", "waiting_user"), step("accel", "pending")],
        }),
      }),
    );
    expect(html).toContain('data-testid="setup-overview"');
    expect(html).toContain("Setup was interrupted when the agent restarted");
    expect(html).toContain("Continue setup");
    expect(html).toContain("0 of 2 done");
  });

  it("draws a card that needs attention with its state", () => {
    const html = render(
      controller({ status: status({ state: "done", steps: [step("frame", "done"), step("battery", "failed")] }) }),
    );
    expect(html).toContain('data-card="battery" data-state="attention"');
    expect(html).toContain('data-card="frame" data-state="done"');
    expect(html).toContain("Run again");
  });

  it("lists the blockers instead of the start button", () => {
    const html = render(
      controller({
        status: status({ available: false, blockers: ["setup.blocker.armed"], steps: [step("frame", "pending")] }),
      }),
    );
    expect(html).toContain("The aircraft is armed");
    expect(html).not.toContain("Start setup");
  });

  it("offers Continue for a step run on its own, even with the wizard filed done and blockers", () => {
    // An abandoned "Apply recommended values" left ados_profile waiting.
    const s = status({
      state: "done",
      available: false,
      blockers: ["setup.blocker.wizard_running"],
      steps: [step("frame", "done"), step("ados_profile", "waiting_user")],
    });
    const html = render(controller({ status: { ...s, running: { wizard: "ground", step: "ados_profile" } } }));
    expect(html).toContain("Continue setup");
    expect(html).not.toContain("Setup cannot start yet");
    expect(html).toContain('data-card="safety" data-state="in_progress"');
    // The run can be aborted from the overview.
    expect(html).toContain("Abort");
  });

  it("opens an asked-for step to read while the other wizard runs, saying why it cannot run", () => {
    const s = status({ available: false, blockers: ["setup.blocker.wizard_running"], steps: [step("frame", "pending")] });
    const air: Wizard = {
      id: "air",
      state: "running",
      available: true,
      blockers: [],
      completed_at: null,
      steps: [{ ...step("autotune", "running"), wizard: "air" }],
    };
    // An alert asked for the ground wizard's frame step while the air run goes on:
    // the step opens to read, with the reason and the way to the run.
    const html = renderWith(
      controller({ status: { ...s, wizards: [s.wizards[0], air], running: { wizard: "air", step: "autotune" } } }),
      { openRequest: { step: "frame", seq: 1 } },
    );
    expect(html).toContain('data-testid="setup-running-elsewhere"');
    expect(html).toContain("Finish it or abort it before running another step.");
    expect(html).toContain(`Go to ${setupText("setup.autotune.title")}`);
  });

  it("marks the Safety card from the pilot's readiness and counts a filed setup as done", () => {
    const html = renderWith(
      controller({ status: status({ state: "done", steps: [step("frame", "pending"), step("ados_profile", "pending")] }) }),
      {
        readiness: [
          {
            id: "fc_profile",
            ok: false,
            severity: "block",
            msg: "FC parameters differ",
            params: [{ name: "MAV_GCS_SYSID", have: 255, want: "1" }],
            fix: { kind: "apply_profile" },
          },
        ],
      },
    );
    expect(html).toContain('data-card="safety" data-state="attention"');
    expect(html).toContain("MAV_GCS_SYSID 255 → 1");
    expect(html).toContain('data-card="frame" data-state="done"');
    expect(html).toContain("2 of 2 done");
  });

  it("names the firmware properly and keeps the FC uid behind Details", () => {
    const s = { ...status({ steps: [step("frame", "pending")] }), firmware: "ardupilot", uid: "3a0021000f51373037363437" };
    const html = render(controller({ status: s }));
    expect(html).toContain("ArduPilot 4.7.3");
    expect(html).not.toContain("capitalize");
    expect(html).toMatch(/<details[^>]*>.*Details.*3a0021000f51373037363437/s);
  });
});

describe("Aircraft setup entry", () => {
  it("is offered on a drone only: a ground station has no setup engine", () => {
    const entry = SCREEN_TABLE.find((s) => s.id === "aircraft-setup");
    expect(entry?.profiles).toEqual(["drone"]);
  });
});
