import { describe, expect, it } from "vitest";

import { ApiError, isAuthChallenge } from "./api";
import { refusalDetail } from "./refusal";

describe("isAuthChallenge", () => {
  it("treats an unpaired node's 403 as a challenge, not a failure", () => {
    // This is the regression. A freshly installed node is unpaired with no PIN
    // set, and answers a data route with 403 and a message naming exactly what
    // it needs. Only 401 used to count, so that 403 fell through to the generic
    // error path and the dashboard told the operator the board was unreachable
    // — about a board that had just replied. The enrolment screen it should
    // have shown already existed and was simply never reached.
    expect(isAuthChallenge(403)).toBe(true);
  });

  it("still treats a paired node's 401 as a challenge", () => {
    expect(isAuthChallenge(401)).toBe(true);
  });

  it("does not swallow codes that mean something is actually wrong", () => {
    // A 500 or a 503 is a fault to surface, not a prompt to ask for a PIN.
    for (const status of [200, 400, 404, 500, 502, 503]) {
      expect(isAuthChallenge(status)).toBe(false);
    }
  });
});

describe("refusalDetail", () => {
  it("returns the agent's own words when it answered and refused", () => {
    const detail =
      "This device is not paired yet. Set up access with the dashboard PIN, or pair it first.";
    const err = new ApiError(`403 ${detail}`, 403, { detail });
    expect(refusalDetail(err)).toBe(detail);
  });

  it("returns null when the agent did not answer at all", () => {
    // "Did not answer" and "answered, and said no" are different faults with
    // different fixes. Reporting the second as the first sends the operator to
    // check a power cable about a board that is running fine.
    expect(refusalDetail(new TypeError("Failed to fetch"))).toBeNull();
    expect(refusalDetail(undefined)).toBeNull();
  });

  it("returns null for a real server fault", () => {
    expect(refusalDetail(new ApiError("500 boom", 500, { detail: "boom" }))).toBeNull();
  });

  it("falls back to a plain statement when the refusal carries no detail", () => {
    expect(refusalDetail(new ApiError("403", 403, null))).toBe(
      "This device refused the request.",
    );
  });
});
