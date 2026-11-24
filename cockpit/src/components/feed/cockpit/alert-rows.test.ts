// @vitest-environment happy-dom
// The shared alert row (`@/shared/hud/alert-rows`) as the cockpit renders
// it: the parameter rows as a table (six, then "Show all N"), the fix for
// each kind, the hold button an ADOS profile fix reaches after reading the
// values it writes, and on a node with no setup engine the hint in place of
// a button.

import { createElement, type ReactElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import "@/shared/i18n";
import type * as SetupClient from "@/shared/setup-wizard/client";
import type { SetupStep } from "@/shared/setup-wizard/types";
import { CHROME_ALERT_SKIN, type AlertActions } from "@/shared/hud/alert-centre";
import { AlertCentre, AlertRow } from "@/shared/hud/alert-rows";
import type { PillAlert } from "@/shared/hud/status-pill";

const profileStep: SetupStep = {
  wizard: "ground",
  step: "ados_profile",
  title_key: "setup.ados_profile.title",
  body_key: "setup.ados_profile.body",
  args: {},
  inputs: [],
  params: [
    { name: "MAV_GCS_SYSID", value: 1 },
    { name: "FS_GCS_ENABLE", value: 5 },
  ],
  image: null,
  state: "waiting_user",
  progress: null,
  live: {},
  actions: [{ id: "apply", label_key: "setup.action.apply", style: "primary", confirm: "none" }],
  blockers: [],
  result: null,
};

const { rerunStep } = vi.hoisted(() => ({ rerunStep: vi.fn() }));
vi.mock("@/shared/setup-wizard/client", async (orig) => ({
  ...(await orig<typeof SetupClient>()),
  rerunStep,
}));
rerunStep.mockImplementation(async () => profileStep);

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

async function render(el: ReactElement): Promise<HTMLElement> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(el));
  return host;
}

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

const params = Array.from({ length: 9 }, (_, i) => ({ name: `PARAM_${i}`, have: i === 0 ? null : i, want: String(i + 1) }));

function alert(over: Partial<PillAlert> = {}): PillAlert {
  return {
    id: "pilot:fc_profile",
    source: "pilot_readiness",
    severity: "critical",
    text: "FC parameters differ",
    group: "setup",
    key: "pilot:fc_profile|FC parameters differ",
    params,
    fix: { kind: "apply_profile" },
    ...over,
  };
}

const actions = (over: Partial<AlertActions> = {}): AlertActions => ({
  openSetup: vi.fn(),
  openSettings: vi.fn(),
  openParameters: vi.fn(),
  ...over,
});

const q = (el: HTMLElement, id: string) => el.querySelector(`[data-testid="${id}"]`);
const qa = (el: HTMLElement, id: string) => el.querySelectorAll(`[data-testid="${id}"]`);

async function click(el: Element | null) {
  await act(async () => (el as HTMLElement).click());
}

describe("alert row", () => {
  it("renders the parameter rows as a table, six then Show all", async () => {
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    const rows = qa(el, "alert-param-row");
    expect(rows).toHaveLength(6);
    expect(rows[0].textContent).toBe("PARAM_0not reported→1");
    expect(rows[1].textContent).toBe("PARAM_11→2");
    expect(q(el, "alert-params-more")?.textContent).toBe("Show all 9");
    await click(q(el, "alert-params-more"));
    expect(qa(el, "alert-param-row")).toHaveLength(9);
  });

  it("an apply_profile fix reads the values it writes, then offers the hold button", async () => {
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    expect(q(el, "alert-fix-apply")?.textContent).toBe("Apply recommended values");
    await click(q(el, "alert-fix-apply"));
    expect(rerunStep).toHaveBeenCalled();
    expect(q(el, "alert-apply-writes")?.textContent).toBe("MAV_GCS_SYSID1FS_GCS_ENABLE5");
    const hold = q(el, "alert-apply-hold")?.querySelector("button");
    expect(hold?.getAttribute("aria-label")).toBe("Apply recommended values. Hold to confirm");
    expect(el.textContent).toContain("A snapshot is saved first");
  });

  it("on a node with no setup engine a setup fix reads as a hint", async () => {
    const el = await render(
      createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions({ openSetup: null }), skin: CHROME_ALERT_SKIN, snoozed: false })),
    );
    expect(q(el, "alert-fix-apply")).toBeNull();
    expect(q(el, "alert-fix-hint")?.textContent).toBe("Fix from the drone's Aircraft setup (Mission Control or the drone's cockpit)");
  });

  it("each fix kind opens its place, and a parameters fix is left out where there is no editor", async () => {
    const a = actions();
    const settings = alert({ fix: { kind: "settings", section: "sensors" }, params: undefined });
    let el = await render(createElement("ul", null, createElement(AlertRow, { alert: settings, actions: a, skin: CHROME_ALERT_SKIN, snoozed: false })));
    await click(q(el, "alert-fix-settings"));
    expect(a.openSettings).toHaveBeenCalledWith("sensors");
    await act(async () => root.unmount());
    host.remove();
    const names = alert({ fix: { kind: "parameters", names: ["FLOW_TYPE"] }, params: undefined });
    el = await render(createElement("ul", null, createElement(AlertRow, { alert: names, actions: actions({ openParameters: null }), skin: CHROME_ALERT_SKIN, snoozed: false })));
    expect(q(el, "alert-fix-parameters")).toBeNull();
  });

  it("a warning offers Hide, a critical Collapse", async () => {
    const onSnooze = vi.fn();
    const warn = alert({ severity: "warning", fix: undefined, params: undefined });
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: warn, actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false, onSnooze })));
    await click(q(el, "alert-hide"));
    expect(onSnooze).toHaveBeenCalledWith(warn.key, true);
  });
});

describe("alert centre", () => {
  it("counts by severity, opens the section holding a critical, and hides snoozed warnings", async () => {
    const crit = alert({ params: undefined });
    const warn: PillAlert = { id: "fcst:EKF variance", source: "fc_status_text", severity: "warning", text: "EKF variance", group: "flight_controller", key: "fcst:EKF variance|EKF variance" };
    const warn2: PillAlert = { ...warn, id: "fcst:Low airspeed", text: "Low airspeed", key: "fcst:Low airspeed|Low airspeed" };
    const el = await render(
      createElement(AlertCentre, { alerts: [crit, warn, warn2], snoozed: new Set([warn.key]), onSnooze: vi.fn(), actions: actions(), skin: CHROME_ALERT_SKIN }),
    );
    expect(q(el, "alert-counts")?.textContent).toBe("1 blocking · 1 warning");
    expect([...qa(el, "alert-section")].map((s) => s.getAttribute("data-group"))).toEqual(["setup", "flight_controller"]);
    // The setup section holds a critical: open. The FC messages: closed.
    expect(qa(el, "alert-item")).toHaveLength(1);
    expect(q(el, "alert-show-hidden")?.textContent).toBe("Show hidden (1)");
    await click(qa(el, "alert-section-toggle")[1]);
    expect([...qa(el, "alert-item-text")].map((t) => t.textContent)).toEqual(["FC parameters differ", "Low airspeed"]);
    await click(q(el, "alert-show-hidden"));
    expect([...qa(el, "alert-item-text")].map((t) => t.textContent)).toEqual(["FC parameters differ", "EKF variance", "Low airspeed"]);
  });

  it("reads all clear with nothing to show", async () => {
    const el = await render(createElement(AlertCentre, { alerts: [], snoozed: new Set<string>(), onSnooze: vi.fn(), actions: actions(), skin: CHROME_ALERT_SKIN }));
    expect(q(el, "alert-all-clear")?.textContent).toContain("All clear: nothing needs attention");
  });
});
