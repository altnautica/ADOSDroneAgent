import { describe, expect, it } from "vitest";

import { ackText } from "@/lib/mav-result";

describe("command acknowledgement words", () => {
  it("prefers the FC's status text, else the spec word for the result, never a raw id", () => {
    expect(ackText({ statustext: "EKF3 source set 2 not allowed", result_name: "DENIED" })).toBe("EKF3 source set 2 not allowed");
    expect(ackText({ statustext: null, result_name: "TEMPORARILY_REJECTED" })).toBe("Temporarily rejected");
    expect(ackText({ result_name: "MAV_RESULT_UNSUPPORTED" })).toBe("Unsupported");
    expect(ackText({ result_name: "SOMETHING_NEW" })).toBe("Refused");
    expect(ackText(null)).toBeUndefined();
  });
});
