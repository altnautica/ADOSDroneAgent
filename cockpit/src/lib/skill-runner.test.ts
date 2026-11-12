import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { COMMAND_TIMEOUT_MS, CommandTimeoutError, sendCommand } from "@/lib/api";
import type { ActionResult } from "@/lib/pilot-link";
import { bypassesBusy, executeSkill, isSkillBusy, requestSkill, useSkillBusyStore } from "@/lib/skill-runner";
import { forgetCommandedModes } from "@/lib/fc-failsafe";
import { BUILTIN_SKILL_BY_ID, resolveSkillState, type Skill, type SkillContext } from "@/lib/skills";
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
    expect(toasts).toEqual(["Resume: Waiting for the last answer"]);
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

/** A plugin motion skill that is running, as its plugin reports it. */
function runningFollow(): { skill: Skill; ctx: SkillContext } {
  const skill: Skill = {
    id: "p:follow",
    label: "Follow",
    category: "behavior",
    confirm: { kind: "tap", ms: 0 },
    armRequirement: "any",
    extension: {
      pluginId: "p",
      pluginName: "P",
      localId: "follow",
      configKey: "active",
      toggle: true,
      stateTopic: "p.state",
      motion: true,
      nudge: false,
      oneShot: false,
      vehicleClasses: ["multirotor"],
      defaultBinding: null,
    },
  };
  const ctx: SkillContext = { ...pilotCtx, reported: { [skill.id]: { state: "active", at: Date.now() } } };
  return { skill, ctx };
}

/** Record each plugin config write; none answers. */
function recordWrites(): unknown[] {
  const bodies: unknown[] = [];
  vi.stubGlobal("fetch", (_url: string, init: RequestInit) => {
    bodies.push(JSON.parse(String(init.body)));
    return new Promise<Response>(() => {});
  });
  return bodies;
}

describe("a plugin motion skill's stop while its start is unanswered", () => {
  it("goes out from a request", () => {
    const { skill, ctx } = runningFollow();
    const writes = recordWrites();
    useSkillBusyStore.setState({ keys: { [`self::${skill.id}`]: true } });
    expect(requestSkill(skill, ctx)).toBe(true);
    expect(writes).toEqual([{ key: "active", value: false }]);
    expect(toasts).toEqual([]);
  });

  it("goes out from the rail's stop slot", async () => {
    const { skill } = runningFollow();
    const writes = recordWrites();
    useSkillBusyStore.setState({ keys: { [`self::${skill.id}`]: true } });
    void executeSkill(skill, { active: true });
    await vi.waitFor(() => expect(writes).toEqual([{ key: "active", value: false }]));
    expect(toasts).toEqual([]);
    // The start it overtook still holds the skill's busy mark.
    expect(isSkillBusy(skill)).toBe(true);
  });
});

describe("pause after this cockpit's own return home on the command route", () => {
  afterEach(() => forgetCommandedModes());

  it("is allowed while the flight controller flies the return it was sent", async () => {
    const stale: SkillContext = {
      fcConnected: true,
      live: true,
      armed: true,
      pilot: { armed: true, landed: false, vehicleClass: "multirotor", fresh: false, fcFailsafe: false },
      pilotLinkOpen: true,
      fc: { mode: "LOITER", systemStatus: 4 },
    };
    ctxNow.ctx = stale;
    const sent: string[] = [];
    vi.stubGlobal("fetch", async (_url: string, init: RequestInit) => {
      sent.push(String(init.body));
      return new Response(JSON.stringify({ status: "ok", ack: { observed: true, accepted: true } }), { status: 200 });
    });
    await executeSkill(B.rth!);
    expect(sent.join()).toContain("rtl");
    expect(resolveSkillState(B.pause!, { ...stale, fc: { mode: "RTL", systemStatus: 4 } }).enabled).toBe(true);
  });

  it("is allowed while a PX4 flight controller flies the landing it was sent", async () => {
    const stale: SkillContext = {
      fcConnected: true,
      live: true,
      armed: true,
      pilot: { armed: true, landed: false, vehicleClass: "multirotor", fresh: false, fcFailsafe: false },
      pilotLinkOpen: true,
      fc: { mode: "AUTO.LOITER", systemStatus: 4 },
    };
    ctxNow.ctx = stale;
    vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ status: "ok", ack: { observed: true, accepted: true } }), { status: 200 }));
    await executeSkill(B.land!);
    expect(resolveSkillState(B.pause!, { ...stale, fc: { mode: "AUTO.LAND", systemStatus: 4 } }).enabled).toBe(true);
  });

  const flying = (mode: string): SkillContext => ({
    fcConnected: true,
    live: true,
    armed: true,
    pilot: { armed: true, landed: false, vehicleClass: "multirotor", fresh: false, fcFailsafe: false },
    pilotLinkOpen: true,
    fc: { mode, systemStatus: 4 },
  });
  const answer = (ack: Record<string, unknown>) => vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ status: "ok", ack }), { status: 200 }));

  it("a refused return claims nothing: a later return is the flight controller's own", async () => {
    ctxNow.ctx = flying("LOITER");
    answer({ observed: true, accepted: false, result_name: "DENIED" });
    await executeSkill(B.rth!);
    expect(resolveSkillState(B.pause!, flying("RTL")).enabled).toBe(false);
  });

  it("a return pressed while the flight controller already flies its own stays its own", async () => {
    ctxNow.ctx = flying("RTL");
    answer({ observed: true, accepted: true });
    await executeSkill(B.rth!);
    expect(resolveSkillState(B.pause!, flying("RTL")).enabled).toBe(false);
  });

  it("is allowed while the flight controller flies the return before its answer arrives", async () => {
    ctxNow.ctx = flying("LOITER");
    let reply: (r: Response) => void = () => {};
    vi.stubGlobal("fetch", () => new Promise<Response>((r) => (reply = r)));
    const run = executeSkill(B.rth!);
    await vi.waitFor(() => expect(resolveSkillState(B.pause!, flying("RTL")).enabled).toBe(true));
    // Refused after all: the return the flight controller flies is its own.
    reply(new Response(JSON.stringify({ status: "ok", ack: { observed: true, accepted: false, result_name: "DENIED" } }), { status: 200 }));
    await run;
    expect(resolveSkillState(B.pause!, flying("RTL")).enabled).toBe(false);
  });

  it("a return the agent could not send claims nothing", async () => {
    ctxNow.ctx = flying("LOITER");
    vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ error: "no flight controller" }), { status: 503 }));
    await executeSkill(B.rth!);
    expect(resolveSkillState(B.pause!, flying("RTL")).enabled).toBe(false);
  });

  it("an unanswered return is ours once the flight controller is seen flying it", async () => {
    ctxNow.ctx = flying("LOITER");
    answer({ observed: false });
    await executeSkill(B.rth!);
    expect(resolveSkillState(B.pause!, flying("RTL")).enabled).toBe(true);
  });
});

describe("the ack line", () => {
  it("clears only the line this outcome wrote, never a later one", async () => {
    vi.useFakeTimers();
    ctxNow.ctx = pilotCtx;
    const done = executeSkill(B.land!);
    actions[0]!.answer({ ok: true });
    await done;
    expect(useConfirmStore.getState().ack?.kind).toBe("ok");
    vi.advanceTimersByTime(1000);
    const later = { text: "Abort refused", kind: "err" as const };
    useConfirmStore.getState().setAck(later);
    vi.advanceTimersByTime(4000);
    expect(useConfirmStore.getState().ack).toBe(later);
  });

  it("clears its own line after four seconds", async () => {
    vi.useFakeTimers();
    ctxNow.ctx = pilotCtx;
    const done = executeSkill(B.land!);
    actions[0]!.answer({ ok: true });
    await done;
    vi.advanceTimersByTime(4000);
    expect(useConfirmStore.getState().ack).toBeNull();
  });
});
