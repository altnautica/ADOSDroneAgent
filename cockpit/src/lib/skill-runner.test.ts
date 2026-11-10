import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { COMMAND_TIMEOUT_MS, CommandTimeoutError, sendCommand } from "@/lib/api";
import type { ActionResult } from "@/lib/pilot-link";
import { bypassesBusy, executeSkill, isSkillBusy, requestSkill, useSkillBusyStore } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, type SkillContext } from "@/lib/skills";
import { onToast } from "@/shared/api/toast";
import { useConfirmStore } from "@/stores/confirm-store";

const ctxNow = vi.hoisted(() => ({ ctx: { fcConnected: true, live: true, armed: true } as SkillContext }));
vi.mock("@/hooks/use-skill-context", () => ({ skillContextNow: () => ctxNow.ctx }));

const actions = vi.hoisted(() => [] as { name: string; answer: (r: ActionResult) => void }[]);
vi.mock("@/lib/pilot-link", () => ({
  // The app's TypeScript lib predates Promise.withResolvers.
  sendPilotAction: (name: string) => new Promise<ActionResult>((answer) => actions.push({ name, answer })),
}));
const B = BUILTIN_SKILL_BY_ID;
const pilotCtx: SkillContext = {
  fcConnected: true,
  live: true,
  armed: true,
  pilot: { armed: true, landed: false, vehicleClass: "multirotor", fresh: true },
  pilotLinkOpen: true,
};

let toasts: string[] = [];
let offToast: () => void = () => {};

beforeEach(() => {
  actions.length = 0;
  toasts = [];
  offToast = onToast((t) => toasts.push(t.message));
  useSkillBusyStore.setState({ keys: {} });
  useConfirmStore.setState({ pending: null, ack: null });
  ctxNow.ctx = pilotCtx;
});

afterEach(() => {
  offToast();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("busy gate", () => {
  it("lets return home, land, pause and kill through whatever is in flight", () => {
    for (const id of ["rth", "land", "pause", "kill"]) expect(bypassesBusy(B[id]!)).toBe(true);
  });

  it("holds the other built-ins only behind their own pending command", () => {
    for (const id of ["arm", "disarm", "takeoff", "resume", "take_control"]) expect(bypassesBusy(B[id]!)).toBe(false);
  });

  it("opens the Return home confirm while another command awaits its answer", () => {
    expect(requestSkill(B.resume!, pilotCtx)).toBe(true);
    expect(actions.map((a) => a.name)).toEqual(["resume"]);
    expect(isSkillBusy(B.resume!)).toBe(true);
    expect(requestSkill(B.rth!, pilotCtx)).toBe(true);
    expect(useConfirmStore.getState().pending?.skill.id).toBe("rth");
    expect(requestSkill(B.land!, pilotCtx)).toBe(true);
    expect(useConfirmStore.getState().pending?.skill.id).toBe("land");
  });

  it("sends Pause and a different skill while one command is pending", () => {
    requestSkill(B.resume!, pilotCtx);
    expect(requestSkill(B.pause!, pilotCtx)).toBe(true);
    expect(requestSkill(B.take_control!, pilotCtx)).toBe(true);
    expect(actions.map((a) => a.name)).toEqual(["resume", "pause", "take_control"]);
  });

  it("says why when a press of the pending skill is dropped", () => {
    requestSkill(B.resume!, pilotCtx);
    expect(requestSkill(B.resume!, pilotCtx)).toBe(false);
    expect(actions).toHaveLength(1);
    expect(toasts).toEqual(["Resume: Another command has control"]);
  });

  it("frees the skill once its answer arrives", async () => {
    requestSkill(B.resume!, pilotCtx);
    actions[0]!.answer({ ok: true });
    await vi.waitFor(() => expect(isSkillBusy(B.resume!)).toBe(false));
    expect(requestSkill(B.resume!, pilotCtx)).toBe(true);
  });

  it("says why when a press of a skill that is not drivable is dropped", () => {
    const landed = { ...pilotCtx, pilot: { ...pilotCtx.pilot!, landed: true, armed: false } };
    expect(requestSkill(B.rth!, landed)).toBe(false);
    expect(toasts).toHaveLength(1);
    expect(toasts[0]).toMatch(/^Return home: \S/);
  });
});

describe("arm and disarm gates on every route", () => {
  it("a press of Arm while airborne opens nothing and says why", () => {
    expect(requestSkill(B.arm!, pilotCtx)).toBe(false);
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(toasts).toEqual(["Arm: Armed; disarm first"]);
  });

  it("a Disarm confirmed after take-off is not sent", async () => {
    await executeSkill(B.disarm!);
    expect(actions).toHaveLength(0);
    expect(toasts).toEqual(["Disarm: Still flying; land first"]);
  });

  it("a Disarm on the ground while armed is sent", async () => {
    ctxNow.ctx = { ...pilotCtx, pilot: { ...pilotCtx.pilot!, landed: true } };
    void executeSkill(B.disarm!);
    expect(actions.map((a) => a.name)).toEqual(["disarm"]);
  });
});

/** A fetch that never answers until aborted. */
function silentFetch(): void {
  vi.stubGlobal(
    "fetch",
    (_url: string, init: RequestInit) =>
      new Promise<Response>((_resolve, reject) => {
        init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
      }),
  );
}

describe("command path timeout", () => {
  it("gives up after the timeout with the no-answer code", async () => {
    vi.useFakeTimers();
    silentFetch();
    const sent = sendCommand("rtl");
    const outcome = expect(sent).rejects.toBeInstanceOf(CommandTimeoutError);
    await vi.advanceTimersByTimeAsync(COMMAND_TIMEOUT_MS);
    await outcome;
  });

  it("reports an unanswered command and frees the skill", async () => {
    vi.useFakeTimers();
    silentFetch();
    ctxNow.ctx = { fcConnected: true, live: true, armed: true, pilot: null };
    const done = executeSkill(B.resume!);
    expect(isSkillBusy(B.resume!)).toBe(true);
    await vi.advanceTimersByTimeAsync(COMMAND_TIMEOUT_MS);
    await done;
    expect(useConfirmStore.getState().ack).toEqual({ text: "Resume: No answer from the drone", kind: "err" });
    expect(isSkillBusy(B.resume!)).toBe(false);
  });
});
