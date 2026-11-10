// The FC route of "Fly here": a reposition through the command route, and its
// answer in the spec's words.

import { describe, expect, it, vi } from "vitest";

// A plain stand-in for the command route (each test sets its answer); it
// records the calls itself.
const route = vi.hoisted(() => ({
  answer: (): Promise<unknown> => Promise.resolve({}),
  calls: [] as unknown[][],
}));
vi.mock("@/lib/api", () => ({
  sendCommand: (...args: unknown[]) => {
    route.calls.push(args);
    return route.answer();
  },
}));

import layoutJson from "@/shared/cockpit-spec/layout.json";
import { flyHereArgs, sendFcReposition } from "@/lib/fly-here";
import { ApiError } from "@/shared/api-fetch";
import { specText } from "@/shared/hud/spec-text";

const point = { lat: 12.9716, lon: 77.5946, alt_m: 25 };
const answers = (body: unknown) => {
  route.answer = () => Promise.resolve(body);
};
const fails = (e: Error) => {
  route.answer = () => Promise.reject(e);
};

describe("fly here through the flight controller", () => {
  it("sends the point and its height above home as a reposition", async () => {
    answers({ status: "ok", ack: { observed: true, accepted: true, result_name: "MAV_RESULT_ACCEPTED" } });
    route.calls = [];
    await expect(sendFcReposition(point)).resolves.toBeNull();
    expect(route.calls).toEqual([["reposition", [12.9716, 77.5946, 25]]]);
  });

  it("reads a refusal as the FC's own words, else its result name from the spec", async () => {
    answers({ ack: { observed: true, accepted: false, result_name: "MAV_RESULT_DENIED" } });
    await expect(sendFcReposition(point)).resolves.toBe(specText("mav_result.DENIED"));
    answers({ ack: { observed: true, accepted: false, result_name: "MAV_RESULT_FAILED", statustext: "Fence breach" } });
    await expect(sendFcReposition(point)).resolves.toBe("Fence breach");
  });

  it("never reads an unanswered command as done", async () => {
    answers({ ack: { observed: false } });
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.no_response"));
    fails(new Error("aborted"));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.no_response"));
  });

  it("names a missing FC link and a refused value", async () => {
    fails(new ApiError("FC not connected", 503, {}));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.no_fc"));
    fails(new ApiError("bad", 400, {}));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.bad_args"));
  });

  it("names an FC that cannot reposition, a denied request and a conflict, never as no answer", async () => {
    fails(new ApiError("not implemented", 501, { detail: "reposition is not supported by this flight controller" }));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.unsupported"));
    fails(new ApiError("forbidden", 403, {}));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.not_authorized"));
    fails(new ApiError("unauthorized", 401, { error: "no_pilot_link" }));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.not_authorized"));
    fails(new ApiError("conflict", 409, { detail: "Another client holds control" }));
    await expect(sendFcReposition(point)).resolves.toBe("Another client holds control");
    fails(new ApiError("conflict", 409, { error: "busy" }));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.busy"));
  });

  it("reads the agent's own 409 code through the refusal table, not its raw detail", async () => {
    fails(new ApiError("conflict", 409, { detail: "the flight controller is in a failsafe", code: "failsafe" }));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.failsafe"));
    fails(new ApiError("conflict", 409, { detail: "not in a state to reposition", code: "bad_state" }));
    await expect(sendFcReposition(point)).resolves.toBe(specText("refusal.bad_state"));
  });
});

describe("fly here height", () => {
  it("never sends a height above the launch ceiling, and keeps the floor", () => {
    const max = layoutJson.launch.alt_max_m;
    expect(flyHereArgs(point, 300)?.alt_m).toBe(max);
    expect(flyHereArgs(point, max + 0.04)?.alt_m).toBe(max);
    expect(flyHereArgs(point, 1)?.alt_m).toBe(3);
    expect(flyHereArgs(point, 50)?.alt_m).toBe(50);
  });
});
