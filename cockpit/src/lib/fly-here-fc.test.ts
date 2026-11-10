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

import { sendFcReposition } from "@/lib/fly-here";
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
});
