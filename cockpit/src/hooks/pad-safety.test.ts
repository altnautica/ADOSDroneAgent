// The pad's emergency stop (L2+R2+Circle held) and return home (Options+Circle
// held) complete on the bare Feed and with a sheet open over it: the frames
// run through the real router, the cockpit actions and the skill runner down
// to the command sender. Circle in those combos never closes the sheet; a
// short Circle tap still does.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { PadRouter, type PadRoute } from "@/hooks/pad-router";
import { registerCockpitActions } from "@/lib/cockpit-actions";
import { BUTTON_INDEX, DEFAULT_BINDINGS, STOP_HOLD_MS, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import type { EngineContext } from "@/lib/pad/gesture-engine";
import type { ActionResult } from "@/lib/pilot-link";
import { runSkillAction, useSkillBusyStore } from "@/lib/skill-runner";
import { BUILTIN_SKILLS, skillActionId, type SkillContext } from "@/lib/skills";
import type { NavCommand } from "@/nav/navigator";
import { useConfirmStore } from "@/stores/confirm-store";
import { useInputStore, type PadSample } from "@/stores/input-store";
import { usePreflightStore } from "@/stores/preflight-store";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";

const airborne: SkillContext = {
  fcConnected: true,
  live: true,
  armed: true,
  pilot: { armed: true, landed: false, vehicleClass: "multirotor", fresh: true },
  pilotLinkOpen: true,
};
vi.mock("@/hooks/use-skill-context", () => ({ skillContextNow: () => airborne }));

const sent = vi.hoisted(() => [] as string[]);
vi.mock("@/lib/pilot-link", () => ({
  sendPilotAction: (name: string) => {
    sent.push(name);
    return new Promise<ActionResult>(() => {});
  },
}));

const ctx: EngineContext = { pickerOpen: false, landed: false, rthActive: false, vehicleVtol: false, nativeSafety: false };

function sample(down: ControlId[]): PadSample {
  const pressed = Array.from({ length: 17 }, () => false);
  for (const c of down) pressed[BUTTON_INDEX[c]!] = true;
  return {
    index: 0,
    id: "pad",
    name: "pad",
    kind: "ds4",
    standard: true,
    axes: [0, 0, 0, 0],
    sticks: { l: [0, 0], r: [0, 0] },
    pressed,
    values: pressed.map((p) => (p ? 1 : 0)),
    l2: down.includes("l2") ? 1 : 0,
    r2: down.includes("r2") ? 1 : 0,
    timestamp: 0,
    polledAt: 0,
  };
}

let sheetOpen = false;
let commands: NavCommand[] = [];
let router: PadRouter;
let offActions: () => void = () => {};

/** Step the router every 50 ms from `from` to `to` with `down` held. */
function hold(down: ControlId[], from: number, to: number): void {
  for (let t = from; t <= to; t += 50) router.step(sample(down), t);
}

beforeEach(() => {
  sent.length = 0;
  commands = [];
  sheetOpen = false;
  useSkillBusyStore.setState({ keys: {} });
  useConfirmStore.setState({ pending: null, ack: null });
  useInputStore.setState({ capture: false, holds: [], pad: null });
  offActions = registerCockpitActions(
    BUILTIN_SKILLS.map((skill) => ({
      id: skillActionId(skill),
      label: skill.label,
      group: "flight" as const,
      run: (phase) => runSkillAction(skill, phase, airborne),
    })),
  );
  router = new PadRouter(DEFAULT_BINDINGS, {
    command: (cmd) => {
      commands.push(cmd);
      if (cmd === "back") sheetOpen = false;
    },
    route: (): PadRoute => (sheetOpen ? "overlay" : "flight"),
    context: () => ctx,
    pad: () => null,
  });
});

afterEach(() => offActions());

describe("pad emergency stop", () => {
  it("sends kill after L2+R2+Circle held on the bare Feed", () => {
    hold(["l2", "r2"], 0, 50);
    hold(["l2", "r2", "circle"], 100, 3200);
    expect(sent).toContain("kill");
  });

  it("sends kill with a sheet open, and the sheet's back never fires", () => {
    sheetOpen = true;
    hold(["l2", "r2"], 0, 50);
    hold(["l2", "r2", "circle"], 100, 3200);
    hold([], 3250, 3300);
    expect(sent).toContain("kill");
    expect(commands).toEqual([]);
    expect(sheetOpen).toBe(true);
  });

  it("sends kill with a sheet open when Circle goes down first", () => {
    sheetOpen = true;
    hold(["circle"], 0, 50);
    hold(["l2", "r2", "circle"], 100, 3200);
    hold([], 3250, 3300);
    expect(sent).toContain("kill");
    expect(commands).toEqual([]);
  });
});

describe("pad return home", () => {
  it("sends rth after Options+Circle held on the bare Feed", () => {
    hold(["options"], 0, 50);
    hold(["options", "circle"], 100, 2200);
    expect(sent).toContain("rth");
  });

  it("sends rth with a sheet open, and neither the Menu nor back fires", () => {
    sheetOpen = true;
    hold(["options"], 0, 50);
    hold(["options", "circle"], 100, 2200);
    hold([], 2250, 2300);
    expect(sent).toContain("rth");
    expect(commands).toEqual([]);
    expect(sheetOpen).toBe(true);
  });
});

describe("Circle tap under a sheet", () => {
  it("still closes the sheet", () => {
    sheetOpen = true;
    hold(["circle"], 0, 100);
    hold([], 150, 200);
    expect(commands).toEqual(["back"]);
    expect(sheetOpen).toBe(false);
    expect(sent).toEqual([]);
  });

  it("an Options tap under a sheet still opens the Menu", () => {
    sheetOpen = true;
    hold(["options"], 0, 100);
    hold([], 150, 200);
    expect(commands).toEqual(["quick-menu"]);
  });
});

describe("a completed pad hold is the confirmation", () => {
  it("sends kill and closes its open sheet", () => {
    const kill = BUILTIN_SKILLS.find((s) => s.id === "kill")!;
    useConfirmStore.getState().open(kill);
    runSkillAction(kill, "hold", airborne);
    expect(sent).toEqual(["kill"]);
    expect(useConfirmStore.getState().pending).toBeNull();
  });
});

describe("a press that ends a capture", () => {
  it("stays with the capture and never also navigates", () => {
    // "Press a control": the editor reads the press from the published sample
    // and leaves capture mode on it.
    sheetOpen = true;
    useInputStore.setState({ capture: true });
    const off = useInputStore.subscribe((s, prev) => {
      if (s.pad?.pressed[0] && !prev.pad?.pressed[0]) s.setCapture(false);
    });
    hold(["cross"], 0, 0);
    hold([], 50, 100);
    off();
    expect(useInputStore.getState().capture).toBe(false);
    expect(commands).toEqual([]);
  });
});

describe("a user row bound to the emergency stop", () => {
  for (const hold_ms of [1, 1500]) {
    it(`sends no kill before the stop hold (row hold ${hold_ms} ms)`, () => {
      const row: PadBinding = { control: "triangle", with: [], gesture: "hold", hold_ms, when: null, action: "kill", rumble: "ramp", locked: false };
      router = new PadRouter([...DEFAULT_BINDINGS.filter((b) => b.control !== "triangle"), row], {
        command: (cmd) => commands.push(cmd),
        route: (): PadRoute => "flight",
        context: () => ctx,
        pad: () => null,
      });
      hold(["triangle"], 0, STOP_HOLD_MS - 100);
      expect(sent).toEqual([]);
      hold(["triangle"], STOP_HOLD_MS, STOP_HOLD_MS);
      expect(sent).toEqual(["kill"]);
    });
  }
});

describe("dispose", () => {
  it("resets the safety engine, so a hold does not carry across it", () => {
    sheetOpen = true;
    hold(["l2", "r2", "circle"], 0, 2000);
    router.dispose();
    hold(["l2", "r2", "circle"], 2050, 3200);
    expect(sent).toEqual([]);
  });
});

describe("a pad arm hold", () => {
  it("opens the pre-flight checklist instead of arming", () => {
    const ground: SkillContext = {
      fcConnected: true,
      live: true,
      armed: false,
      pilot: { armed: false, landed: true, vehicleClass: "multirotor", fresh: true },
      pilotLinkOpen: true,
    };
    usePreflightStore.setState({ passed: false, pending: null });
    const arm = BUILTIN_SKILLS.find((s) => s.id === "arm")!;
    runSkillAction(arm, "hold", ground);
    expect(sent).toEqual([]);
    expect(usePreflightStore.getState().pending).not.toBeNull();
    expect(useCockpitUiStore.getState().sheet).toBe("preflight");
  });
});
