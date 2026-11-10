import { beforeEach, describe, expect, it, vi } from "vitest";

const sent = vi.hoisted(() => [] as string[]);
const requested = vi.hoisted(() => [] as string[]);
vi.mock("@/lib/skill-runner", async () => {
  const { useConfirmStore } = await import("@/stores/confirm-store");
  return {
    executeSkill: async (skill: { id: string }) => {
      sent.push(skill.id);
    },
    // The STOP button's own path: gate, then open the confirm sheet.
    requestSkill: (skill: Parameters<ReturnType<typeof useConfirmStore.getState>["open"]>[0]) => {
      requested.push(skill.id);
      useConfirmStore.getState().open(skill);
      return true;
    },
  };
});
vi.mock("@/hooks/use-skill-context", () => ({ skillContextNow: () => ({}) }));

import { BUILTIN_SKILLS } from "@/lib/skills";
import { STOP_HOLD_MS } from "@/lib/pad/ds4-map";
import { useConfirmStore } from "@/stores/confirm-store";
import type { NavCommand } from "@/nav/navigator";

import {
  PANEL_LONG_PRESS_MS,
  PANEL_STOP_ARM_MS,
  dispatchPanelEvent,
  eventToCommand,
  isLongSelect,
  panelLongPressCompletes,
} from "./use-buttons";

describe("eventToCommand", () => {
  it("maps a bound button", () => {
    expect(eventToCommand({ button: "17", label: "b3", kind: "short", action: "cycle_screen" })).toBe(
      "activate",
    );
  });

  it("leaves a passthrough button to plugins", () => {
    expect(eventToCommand({ button: "5", label: "b1", kind: "short", action: "passthrough" })).toBeNull();
    expect(eventToCommand({ button: "5", label: "b1", kind: "long", action: "passthrough" })).toBeNull();
  });

  it("steps with the encoder and fires or goes back with its switch", () => {
    const enc = (label: string, kind: string) =>
      eventToCommand({ button: "130", label, kind, action: "cycle_screen" });
    expect(enc("enc_cw", "short")).toBe("next");
    expect(enc("ENC_CCW", "short")).toBe("prev");
    expect(enc("enc_sw", "short")).toBe("activate");
    expect(enc("enc_sw", "long")).toBe("back");
    expect(enc("enc_sw", "cancel")).toBeNull();
  });
});

describe("panel long-press as a completed hold", () => {
  it("completes a hold no longer than the long press", () => {
    expect(panelLongPressCompletes(1500)).toBe(true);
    expect(panelLongPressCompletes(PANEL_LONG_PRESS_MS)).toBe(true);
  });

  it("never completes the emergency stop", () => {
    const kill = BUILTIN_SKILLS.find((s) => s.id === "kill")!;
    expect(kill.confirm.ms).toBe(STOP_HOLD_MS);
    expect(panelLongPressCompletes(kill.confirm.ms)).toBe(false);
  });
});

describe("the panel's emergency stop", () => {
  const longSelect = { button: "17", label: "b3", kind: "long", action: "cycle_screen" };
  let now = 0;
  let stopFocused = true;
  const commands: NavCommand[] = [];
  const deps = {
    command: (c: NavCommand) => commands.push(c),
    stopFocused: () => stopFocused,
    now: () => now,
  };

  beforeEach(() => {
    sent.length = 0;
    requested.length = 0;
    commands.length = 0;
    stopFocused = true;
    useConfirmStore.getState().cancel();
    vi.spyOn(performance, "now").mockImplementation(() => now);
    now = 10_000;
  });

  it("opens the STOP sheet on one long press and sends nothing", () => {
    dispatchPanelEvent(longSelect, deps);
    expect(useConfirmStore.getState().pending?.skill.id).toBe("kill");
    expect(sent).toEqual([]);
    expect(commands).toEqual([]);
  });

  it("fires the stop on a second long press once the sheet has been open a moment", () => {
    dispatchPanelEvent(longSelect, deps);
    now += PANEL_STOP_ARM_MS - 1;
    dispatchPanelEvent(longSelect, deps);
    expect(sent).toEqual([]);
    expect(useConfirmStore.getState().pending?.skill.id).toBe("kill");
    now += 1;
    dispatchPanelEvent(longSelect, deps);
    expect(sent).toEqual(["kill"]);
    expect(useConfirmStore.getState().pending).toBeNull();
  });

  it("cancels the STOP sheet on back", () => {
    dispatchPanelEvent(longSelect, deps);
    dispatchPanelEvent({ button: "4", label: "b4", kind: "short", action: "back" }, deps);
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(sent).toEqual([]);
  });

  it("opens the quick menu on a long press away from STOP", () => {
    stopFocused = false;
    dispatchPanelEvent(longSelect, deps);
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(commands).toEqual(["quick-menu"]);
  });

  const longBack = { button: "4", label: "b4", kind: "long", action: "menu" };

  it("opens the STOP sheet through the STOP button's gate", () => {
    dispatchPanelEvent(longSelect, deps);
    expect(requested).toEqual(["kill"]);
  });

  it("closes the STOP sheet on a long back and never fires", () => {
    dispatchPanelEvent(longSelect, deps);
    now += PANEL_STOP_ARM_MS * 5;
    dispatchPanelEvent(longBack, deps);
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(sent).toEqual([]);
    dispatchPanelEvent(longBack, deps);
    expect(sent).toEqual([]);
  });

  it("fires on two long selects a second apart", () => {
    dispatchPanelEvent(longSelect, deps);
    now += PANEL_STOP_ARM_MS;
    dispatchPanelEvent(longSelect, deps);
    expect(sent).toEqual(["kill"]);
  });

  it("keeps long back as the menu with STOP focused, never the STOP sheet", () => {
    dispatchPanelEvent(longBack, deps);
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(requested).toEqual([]);
    expect(commands).toEqual(["quick-menu"]);
  });

  it("counts only select toward the STOP", () => {
    expect(isLongSelect(longSelect)).toBe(true);
    expect(isLongSelect(longBack)).toBe(false);
    expect(isLongSelect({ button: "130", label: "enc_sw", kind: "long", action: "x" })).toBe(false);
    expect(isLongSelect({ ...longSelect, kind: "short" })).toBe(false);
  });

  it("takes two long selects on a sheet opened another way (short select, touch, pad)", () => {
    const kill = BUILTIN_SKILLS.find((x) => x.id === "kill")!;
    useConfirmStore.getState().open(kill);
    now += PANEL_STOP_ARM_MS * 3;
    dispatchPanelEvent(longSelect, deps);
    expect(sent).toEqual([]);
    expect(useConfirmStore.getState().pending?.skill.id).toBe("kill");
    now += PANEL_STOP_ARM_MS - 1;
    dispatchPanelEvent(longSelect, deps);
    expect(sent).toEqual([]);
    now += 1;
    dispatchPanelEvent(longSelect, deps);
    expect(sent).toEqual(["kill"]);
  });
});
