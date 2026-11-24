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
import { ApiError } from "@/shared/api-fetch";
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
  args: { reboots: true },
  inputs: [],
  params: [
    { name: "MAV_GCS_SYSID", value: 1, have: 255 },
    { name: "FS_GCS_ENABLE", value: 5, have: 5 },
    { name: "FS_EKF_THRESH", value: 0.8, have: 0.800000011920929 },
  ],
  image: null,
  state: "waiting_user",
  progress: null,
  live: {},
  actions: [{ id: "apply", label_key: "setup.action.apply", style: "primary", confirm: "none" }],
  blockers: [],
  result: null,
};

const { rerunStep, abortWizard, fetchSetupStatus } = vi.hoisted(() => ({ rerunStep: vi.fn(), abortWizard: vi.fn(), fetchSetupStatus: vi.fn() }));
vi.mock("@/shared/setup-wizard/client", async (orig) => ({
  ...(await orig<typeof SetupClient>()),
  rerunStep,
  abortWizard,
  fetchSetupStatus,
}));
rerunStep.mockImplementation(async () => profileStep);
abortWizard.mockImplementation(async () => ({}));

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

/** Click, then drain the microtasks of the request chain the click starts
 *  (mocked calls resolve at once; a refusal falls back to a status read). */
async function click(el: Element | null) {
  await act(async () => {
    (el as HTMLElement).click();
    for (let i = 0; i < 10; i++) await Promise.resolve();
  });
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

  it("titles a row with a table by its short words, keeping the full text for screen readers", async () => {
    const long = alert({ text: "FC parameters differ: MAV_GCS_SYSID 255 (want 1)" });
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: long, actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    expect(q(el, "alert-item-text")?.textContent).toBe("FC parameters differ");
    expect(q(el, "alert-item-text")?.getAttribute("aria-hidden")).toBe("true");
    expect(q(el, "alert-item-fulltext")?.textContent).toBe(long.text);
    expect(q(el, "alert-item-fulltext")?.className).toContain("sr-only");
  });

  it("an apply_profile fix shows only the changes as current → new, folds the rest, warns of the restart, then offers the hold", async () => {
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    expect(q(el, "alert-fix-apply")?.textContent).toBe("Apply recommended values");
    await click(q(el, "alert-fix-apply"));
    expect(rerunStep).toHaveBeenCalled();
    expect([...qa(el, "alert-apply-change")].map((r) => r.textContent)).toEqual(["MAV_GCS_SYSID255→1"]);
    expect(qa(el, "alert-apply-same")).toHaveLength(0);
    expect(q(el, "alert-apply-same-toggle")?.textContent).toBe("2 already set");
    await click(q(el, "alert-apply-same-toggle"));
    expect(qa(el, "alert-apply-same")).toHaveLength(2);
    expect(q(el, "alert-apply-reboots")?.textContent).toBe("The flight controller will restart");
    const hold = q(el, "alert-apply-hold")?.querySelector("button");
    expect(hold?.getAttribute("aria-label")).toBe("Apply recommended values. Hold to confirm");
    expect(el.textContent).toContain("A snapshot is saved first");
  });

  it("hides the row's own table while the review is open, so the review list is the only one, and restores it on Cancel", async () => {
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    expect(qa(el, "alert-param-row")).toHaveLength(6);
    await click(q(el, "alert-fix-apply"));
    expect(q(el, "alert-params")).toBeNull();
    expect(qa(el, "alert-apply-change").length).toBeGreaterThan(0);
    await click(q(el, "alert-apply-cancel"));
    expect(qa(el, "alert-param-row")).toHaveLength(6);
  });

  it("Cancel, or the row going away mid-review, aborts the ground wizard's open step", async () => {
    abortWizard.mockClear();
    let el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    await click(q(el, "alert-fix-apply"));
    await click(q(el, "alert-apply-cancel"));
    expect(abortWizard).toHaveBeenCalledWith("ground");
    expect(q(el, "alert-fix-apply")).not.toBeNull();
    await act(async () => root.unmount());
    host.remove();
    abortWizard.mockClear();
    await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    await act(async () => root.unmount());
    expect(abortWizard).not.toHaveBeenCalled();
    host.remove();
    el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    await click(q(el, "alert-fix-apply"));
    await act(async () => root.unmount());
    expect(abortWizard).toHaveBeenCalledWith("ground");
    await render(createElement("div"));
  });

  it("picks up the review an earlier Apply left open instead of failing on wizard_running", async () => {
    rerunStep.mockImplementationOnce(async () => {
      throw new ApiError("409", 409, { ok: false, err: "wizard_running" });
    });
    fetchSetupStatus.mockImplementationOnce(async () => ({
      uid: "u",
      firmware: null,
      firmware_version: null,
      vehicle_class: null,
      running: { wizard: "ground", step: "ados_profile" },
      wizards: [{ id: "ground", state: "done", available: true, blockers: [], completed_at: null, steps: [profileStep] }],
      report: null,
    }));
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    await click(q(el, "alert-fix-apply"));
    expect(q(el, "alert-apply-error")).toBeNull();
    expect(q(el, "alert-apply-review")).not.toBeNull();
    expect(q(el, "alert-apply-hold")).not.toBeNull();
  });

  it("words a refusal through setup.err, never the raw code", async () => {
    rerunStep.mockImplementationOnce(async () => {
      throw new ApiError("409", 409, { ok: false, err: "wizard_running" });
    });
    fetchSetupStatus.mockImplementationOnce(async () => ({
      uid: "u",
      firmware: null,
      firmware_version: null,
      vehicle_class: null,
      running: { wizard: "air", step: "hover" },
      wizards: [],
      report: null,
    }));
    const el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false })));
    await click(q(el, "alert-fix-apply"));
    expect(q(el, "alert-apply-error")?.textContent).toContain("Another wizard is running");
    expect(el.textContent).not.toContain("wizard_running");
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

  it("puts Hide or Collapse on the title line, with no action line under a row that has no fix", async () => {
    const warn = alert({ severity: "warning", fix: undefined, params: undefined });
    let el = await render(
      createElement("ul", null, createElement(AlertRow, { alert: warn, actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false, onSnooze: vi.fn() })),
    );
    const titleLine = q(el, "alert-item-text")?.parentElement?.parentElement;
    expect(titleLine?.contains(q(el, "alert-hide"))).toBe(true);
    expect(titleLine?.nextElementSibling).toBeNull();
    expect(q(el, "alert-hide")?.className).toContain("min-h-[48px]");
    await act(async () => root.unmount());
    host.remove();
    el = await render(createElement("ul", null, createElement(AlertRow, { alert: alert(), actions: actions(), skin: CHROME_ALERT_SKIN, snoozed: false, onSnooze: vi.fn() })));
    expect(q(el, "alert-item-text")?.parentElement?.parentElement?.contains(q(el, "alert-collapse"))).toBe(true);
  });

  it("sizes every word in px against the 16 px root with its own leading, so wrapped lines never overlap", async () => {
    const el = await render(createElement("ul", null, createElement(AlertCentre, { alerts: [alert()], snoozed: new Set<string>(), onSnooze: vi.fn(), actions: actions(), skin: CHROME_ALERT_SKIN })));
    expect(q(el, "alert-item")?.className).toContain("text-[14px] leading-[1.3]");
    expect(q(el, "alert-item-source")?.className).toContain("text-[12.5px] leading-[1.3]");
    // No size without a leading beside it, and no size relative to the panel's
    // fixed 15 px line or to the root (the cockpit zooms the popover instead).
    for (const node of el.querySelectorAll("[class*='text-[']")) {
      const cls = node.getAttribute("class") ?? "";
      if (/text-\[[0-9.]+px\]/.test(cls)) expect(cls, cls).toMatch(/leading-\[/);
    }
    expect(el.innerHTML).not.toMatch(/text-\[[0-9.]+(em|rem)\]/);
    expect(el.innerHTML).not.toMatch(/text-\[length:/);
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
