import { createElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { sendCommand, type CommandResponse } from "@/lib/api";
import type * as Api from "@/lib/api";
import { demoPilotData } from "@/lib/mock-layers";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import { specText } from "@/shared/hud/spec-text";
import { useNavStreamStore, type NavStreamState } from "@/stores/nav-stream-store";

import { fcFailsafeForPause, FC_RECOVERY_MODES, forgetCommandedModes } from "@/lib/fc-failsafe";
import { fcModesFor } from "@/lib/fc-modes";
import { MODE_SHEET } from "@/lib/cockpit-spec";

import { ModeSheet } from "./mode-sheet";
import { modeChange, pickDisabled, sendFcMode } from "./mode-sheet-pending";

vi.mock("@/components/ui/sheet", () => ({
  Sheet: ({ children, footer }: { children: ReactNode; footer?: ReactNode }) => createElement("div", null, children, footer),
}));

vi.mock("@/lib/api", async (orig) => ({ ...(await orig<typeof Api>()), sendCommand: vi.fn() }));

describe("a pending pick in the Mode sheet", () => {
  beforeEach(() => vi.mocked(sendCommand).mockReset());

  it("never holds back the recovery modes or handing to the FC", () => {
    const pending = { profile: "sport" };
    for (const mode of ["RTL", "SMART_RTL", "LAND", "BRAKE", "QRTL", "QLAND"]) expect(pickDisabled(pending, "fc", mode)).toBe(false);
    expect(pickDisabled({ fc: "LOITER" }, "fc", "RTL")).toBe(false);
    expect(pickDisabled({ who: "take" }, "who", "loiter")).toBe(false);
  });

  it("one recovery set: Loiter, Alt Hold and PosHold are never held back and take the short hold", () => {
    for (const mode of ["LOITER", "ALT_HOLD", "POSHOLD"]) {
      expect(FC_RECOVERY_MODES[mode], mode).toBe(true);
      expect(pickDisabled({ fc: "AUTO" }, "fc", mode), mode).toBe(false);
    }
    for (const family of ["copter", "plane", "px4"] as const) {
      for (const m of fcModesFor(family)) expect(m.holdMs, `${family} ${m.name}`).toBe(FC_RECOVERY_MODES[m.name] ? 800 : 1500);
    }
    expect(fcModesFor("copter").find((m) => m.name === "GUIDED")?.holdMs).toBe(1500);
  });

  it("waits only in its own row", () => {
    const pending = { profile: "sport" };
    expect(pickDisabled(pending, "profile", "cine")).toBe(true);
    expect(pickDisabled(pending, "avoid", "off")).toBe(false);
    expect(pickDisabled(pending, "fc", "AUTO")).toBe(false);
    expect(pickDisabled({ fc: "LOITER" }, "fc", "AUTO")).toBe(true);
  });

  it("sends the return home while stopping the motion skills hangs", async () => {
    const accepted: CommandResponse = { ack: { observed: true, accepted: true } };
    vi.mocked(sendCommand).mockResolvedValue(accepted);
    const hung = vi.fn(() => new Promise<void>(() => {}));
    expect(pickDisabled({ profile: "sport" }, "fc", "RTL")).toBe(false);
    await expect(modeChange(() => sendFcMode("RTL"), hung)).resolves.toEqual({ ok: true });
    expect(hung).toHaveBeenCalledOnce();
    expect(sendCommand).toHaveBeenCalledWith("mode", ["RTL"]);
  });

  it("lets a pause through while the return it sent awaits its answer", async () => {
    forgetCommandedModes();
    let reply: (r: CommandResponse) => void = () => {};
    vi.mocked(sendCommand).mockReturnValue(new Promise<CommandResponse>((r) => (reply = r)));
    const sent = sendFcMode("RTL");
    expect(fcFailsafeForPause({ mode: "RTL", systemStatus: 4 })).toBe(false);
    reply({ ack: { observed: true, accepted: false, result_name: "DENIED" } });
    await sent;
    expect(fcFailsafeForPause({ mode: "RTL", systemStatus: 4 })).toBe(true);
    forgetCommandedModes();
  });
});

/** A server render reads a store's initial state, so the pilot goes there. */
function serverPilot(pilot: NavStreamState["pilot"]): void {
  Object.assign(useNavStreamStore.getInitialState(), { pilot });
}

describe("the Mode sheet on a stale pilot frame", () => {
  const pilot = parsePilotState({ ...demoPilotData(0), owner: "fc_mode", profile: "sport", avoid: "off" })!;
  afterEach(() => serverPilot(null));

  const render = (ageMs: number) => {
    serverPilot({ data: pilot, at: Date.now() - ageMs });
    return renderToStaticMarkup(createElement(ModeSheet));
  };
  /** The attributes of the Take control button. */
  const takeControl = (html: string) =>
    html.match(new RegExp(`<button([^>]*)>(?:(?!</button>).)*${specText("flow.mode.take_control").replace(/[()]/g, "\\$&")}`))?.[1];

  it("shows the current values and offers Take control while the frame is fresh", () => {
    const html = render(200);
    expect(html).toContain('aria-checked="true"');
    expect(html).toContain(specText("flow.mode.avoid_off_warning"));
    expect(html).not.toContain('data-testid="mode-stale"');
    expect(takeControl(html)).not.toContain('disabled=""');
  });

  it("shows no current value and holds Take control back on a 2.5 s old frame", () => {
    const html = render(2500);
    expect(html).not.toContain('aria-checked="true"');
    expect(html).not.toContain(specText("flow.mode.avoid_off_warning"));
    expect(html).toContain('data-testid="mode-stale"');
    expect(html).toContain(specText("flow.mode.stale"));
    expect(takeControl(html)).toContain('disabled=""');
  });
});

describe("the Mode sheet's control mode row on a drone", () => {
  afterEach(() => serverPilot(null));
  const render = (controlMode: string) => {
    serverPilot({ data: parsePilotState({ ...demoPilotData(0), control_mode: controlMode })!, at: Date.now() });
    return renderToStaticMarkup(createElement(ModeSheet));
  };
  const elrsExplain = MODE_SHEET.control_mode.explain.elrs.replace(/'/g, "&#x27;");

  it("keeps ELRS, selected and explained, while it is the current mode", () => {
    const html = render("elrs");
    expect(html).toMatch(/aria-checked="true"[^>]*>(?:(?!<\/button>).)*ELRS/);
    expect(html).toContain(elrsExplain);
  });

  it("does not offer ELRS on a drone otherwise", () => {
    expect(render("ados")).not.toContain(elrsExplain);
  });
});
