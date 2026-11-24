import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { SCREEN_TABLE } from "@/nav/screen-table";
import "@/shared/i18n";
import { setupText } from "@/shared/setup-wizard/strings";
import type { SetupStatus, SetupStep, StepState, Wizard } from "@/shared/setup-wizard/types";
import type { FcSetupController } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView } from "@/shared/setup-wizard/wizard-view";

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
});

describe("Aircraft setup entry", () => {
  it("is offered on a drone only: a ground station has no setup engine", () => {
    const entry = SCREEN_TABLE.find((s) => s.id === "aircraft-setup");
    expect(entry?.profiles).toEqual(["drone"]);
  });
});
