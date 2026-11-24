// @vitest-environment happy-dom
// The Aircraft setup screen mounted live: open requests that arrive while it
// is already showing, the reason a step cannot run while another runs, the
// confirms in front of Run again and Abort, and the status re-read a stream
// frame triggers after an agent restart.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "@/shared/i18n";
import type * as SetupClient from "@/shared/setup-wizard/client";
import type { SetupFrame, SetupStatus, SetupStep, StepState, Wizard } from "@/shared/setup-wizard/types";
import { useFcSetup, type FcSetupController } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView, type WizardViewProps } from "@/shared/setup-wizard/wizard-view";

const client = vi.hoisted(() => ({
  fetchSetupStatus: vi.fn<() => Promise<SetupStatus>>(),
  onFrame: null as ((frame: SetupFrame) => void) | null,
}));

vi.mock("@/shared/setup-wizard/client", async (importOriginal) => {
  const real = await importOriginal<typeof SetupClient>();
  return {
    ...real,
    fetchSetupStatus: client.fetchSetupStatus,
    openSetupStream: (onFrame: (frame: SetupFrame) => void) => {
      client.onFrame = onFrame;
      return () => undefined;
    },
  };
});

// React's act() checks this flag before it flushes updates.
Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

function step(id: string, state: StepState): SetupStep {
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
  };
}

function status(ground: Partial<Wizard>, running: SetupStatus["running"] = null): SetupStatus {
  return {
    uid: "fc-1",
    firmware: "ardupilot",
    firmware_version: "4.7.3",
    vehicle_class: "multirotor",
    running,
    report: null,
    wizards: [{ id: "ground", state: "done", available: true, blockers: [], completed_at: null, steps: [], ...ground }],
  };
}

function controller(over: Partial<FcSetupController>): FcSetupController {
  return {
    status: null,
    loading: false,
    streamUp: true,
    loadError: null,
    requestError: null,
    busy: null,
    refresh: async () => undefined,
    start: vi.fn(async () => true),
    resume: async () => true,
    abort: vi.fn(async () => undefined),
    action: async () => undefined,
    rerun: vi.fn(async () => true),
    revert: async () => null,
    ...over,
  };
}

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

async function show(props: WizardViewProps): Promise<void> {
  await act(async () => root.render(createElement(WizardView, props)));
}

function button(name: string): HTMLButtonElement {
  const b = [...host.querySelectorAll("button")].find((el) => el.textContent?.includes(name));
  if (!b) throw new Error(`no button "${name}" in ${host.textContent}`);
  return b;
}

const steps = [step("frame", "done"), step("battery", "done"), step("ados_profile", "done")];

describe("open requests on a mounted screen", () => {
  it("opens each new request's step, also when the screen is already showing", async () => {
    const setup = controller({ status: status({ steps }) });
    const handled = vi.fn();
    await show({ setup, onOpenHandled: handled });
    expect(setup.rerun).not.toHaveBeenCalled();

    await show({ setup, onOpenHandled: handled, openRequest: { step: "frame", seq: 1 } });
    expect(setup.rerun).toHaveBeenLastCalledWith(expect.objectContaining({ step: "frame" }));
    expect(handled).toHaveBeenLastCalledWith(1);
    expect(host.querySelector('[data-testid="setup-guided"]')).not.toBeNull();

    // A second "Open setup" while the first step is still on screen.
    await show({ setup, onOpenHandled: handled, openRequest: { step: "ados_profile", seq: 2 } });
    expect(setup.rerun).toHaveBeenLastCalledWith(expect.objectContaining({ step: "ados_profile" }));
    expect(handled).toHaveBeenLastCalledWith(2);
  });

  it("says why a step cannot run while a run holds another, with the way there", async () => {
    const live = [step("frame", "done"), step("ados_profile", "waiting_user")];
    const setup = controller({ status: status({ steps: live }, { wizard: "ground", step: "ados_profile" }) });
    await show({ setup, openRequest: { step: "frame", seq: 1 } });
    expect(setup.rerun).not.toHaveBeenCalled();
    const note = host.querySelector('[data-testid="setup-running-elsewhere"]');
    expect(note?.textContent).toContain("Finish it or abort it");
    // "Go to" lands on the running step.
    await act(async () => button("Go to").click());
    expect(host.querySelector('[data-testid="setup-running-elsewhere"]')).toBeNull();
  });

  it("offers the way to a run in the other wizard beside the blocker it causes", async () => {
    const air: Wizard = {
      id: "air",
      state: "running",
      available: true,
      blockers: [],
      completed_at: null,
      steps: [{ ...step("autotune", "running"), wizard: "air" }],
    };
    const s = status({ state: "done", available: false, blockers: ["setup.blocker.wizard_running"], steps });
    const setup = controller({ status: { ...s, wizards: [s.wizards[0], air], running: { wizard: "air", step: "autotune" } } });
    await show({ setup });
    // The tab of the aircraft setup, while flight tuning runs.
    await act(async () => button("Aircraft setup").click());
    expect(host.textContent).toContain("Another setup wizard is running");
    await act(async () => button("Go to").click());
    expect(host.querySelector('[data-testid="setup-guided"]')).not.toBeNull();
    expect(setup.rerun).not.toHaveBeenCalled();
  });
});

describe("confirms", () => {
  it("asks before Run again clears a finished setup", async () => {
    const setup = controller({ status: status({ steps }) });
    await show({ setup });
    await act(async () => button("Run again").click());
    expect(host.querySelector('[data-testid="setup-confirm"]')?.textContent).toContain("Start this setup over?");
    expect(setup.start).not.toHaveBeenCalled();
    await act(async () => button("Cancel").click());
    expect(host.querySelector('[data-testid="setup-confirm"]')).toBeNull();
    expect(setup.start).not.toHaveBeenCalled();
  });

  it("asks before Abort stops the ground setup", async () => {
    const setup = controller({ status: status({ state: "running", steps: [step("frame", "waiting_user")] }, { wizard: "ground", step: "frame" }) });
    await show({ setup });
    await act(async () => button("Abort").click());
    expect(host.querySelector('[data-testid="setup-confirm"]')?.textContent).toContain("Abort setup?");
    expect(setup.abort).not.toHaveBeenCalled();
  });
});

describe("status after an agent restart", () => {
  afterEach(() => {
    vi.useRealTimers();
    client.fetchSetupStatus.mockReset();
  });

  it("re-reads the full status on a stream frame while the last read had no FC", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(1_000_000);
    const restarting = { ...status({ steps }), uid: null, firmware: null };
    const identified = status({ state: "interrupted", steps });
    client.fetchSetupStatus.mockResolvedValueOnce(restarting).mockResolvedValue(identified);
    let seen: SetupStatus | null = null;
    function Probe() {
      seen = useFcSetup().status;
      return null;
    }
    await act(async () => root.render(createElement(Probe)));
    expect(seen!.uid).toBeNull();
    expect(client.fetchSetupStatus).toHaveBeenCalledTimes(1);

    // A keepalive right away does not re-read (at most one read per 2 s)…
    await act(async () => client.onFrame!({ kind: "keepalive", ts_ms: 1 }));
    expect(client.fetchSetupStatus).toHaveBeenCalledTimes(1);
    // …one 2 s later does, and the screen gets the FC and the interrupted run.
    vi.setSystemTime(1_002_500);
    await act(async () => client.onFrame!({ kind: "keepalive", ts_ms: 2 }));
    expect(client.fetchSetupStatus).toHaveBeenCalledTimes(2);
    expect(seen!.uid).toBe("fc-1");
    expect(seen!.wizards[0].state).toBe("interrupted");
    // With the FC known, frames fold in without further reads.
    vi.setSystemTime(1_010_000);
    await act(async () => client.onFrame!({ kind: "keepalive", ts_ms: 3 }));
    expect(client.fetchSetupStatus).toHaveBeenCalledTimes(2);
  });
});
