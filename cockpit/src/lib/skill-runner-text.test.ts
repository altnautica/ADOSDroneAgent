// A command's outcome and a refusal read in the spec's words (`mav_result.*`,
// `refusal.*`, `skill.*`), never as a raw result name, code or English literal.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { executeSkill, useSkillBusyStore } from "@/lib/skill-runner";
import { armGateReason, BUILTIN_SKILL_BY_ID, resolveSkillState, type SkillContext } from "@/lib/skills";
import { useConfirmStore } from "@/stores/confirm-store";

const ctxNow = vi.hoisted(() => ({ ctx: { fcConnected: true, live: true, armed: true, pilot: null } as SkillContext }));
vi.mock("@/hooks/use-skill-context", () => ({ skillContextNow: () => ctxNow.ctx }));

const B = BUILTIN_SKILL_BY_ID;

function answer(status: number, body: unknown): void {
  vi.stubGlobal("fetch", async () => new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }));
}

beforeEach(() => {
  useSkillBusyStore.setState({ keys: {} });
  useConfirmStore.setState({ pending: null, ack: null });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("command outcome text", () => {
  it("names a flight controller rejection by its spec text", async () => {
    answer(200, { status: "error", ack: { observed: true, result: 2, result_name: "DENIED", accepted: false } });
    await executeSkill(B.rth!);
    expect(useConfirmStore.getState().ack).toEqual({ text: "Return home: Denied", kind: "err" });
  });

  it("keeps the flight controller's own message after the result", async () => {
    answer(200, { ack: { observed: true, result: 4, result_name: "FAILED", accepted: false, statustext: "PreArm: GPS" } });
    await executeSkill(B.land!);
    expect(useConfirmStore.getState().ack?.text).toBe("Land: Failed — PreArm: GPS");
  });

  it("says Accepted for an accepted command", async () => {
    answer(200, { ack: { observed: true, result: 0, result_name: "ACCEPTED", accepted: true } });
    await executeSkill(B.rth!);
    expect(useConfirmStore.getState().ack).toEqual({ text: "Return home: Accepted", kind: "ok" });
  });

  it("says the skill did not respond when nothing acknowledged it", async () => {
    answer(200, { status: "ok", ack: { observed: false } });
    await executeSkill(B.resume!);
    expect(useConfirmStore.getState().ack).toEqual({ text: "Resume did not respond", kind: "warn" });
  });

  it("names a missing flight controller link in the spec's words", async () => {
    answer(503, { detail: "FC not connected" });
    await executeSkill(B.rth!);
    expect(useConfirmStore.getState().ack).toEqual({ text: "Return home: No flight controller", kind: "err" });
  });
});

describe("gate reasons", () => {
  it("reads in the spec's refusal words", () => {
    expect(armGateReason("armed", false, true)).toBe("Not armed");
    expect(armGateReason("disarmed", true, true)).toBe("Armed; disarm first");
    expect(armGateReason("armed_landed", true, false)).toBe("Still flying; land first");
    const noPilot: SkillContext = { fcConnected: true, live: true, armed: true, pilot: null };
    expect(resolveSkillState(B.take_control!, noPilot).reason).toBe("ADOS Pilot is not running");
    const noLink: SkillContext = { fcConnected: false, live: false, armed: false, pilot: null };
    expect(resolveSkillState(B.rth!, noLink).reason).toBe("No flight controller");
  });
});
