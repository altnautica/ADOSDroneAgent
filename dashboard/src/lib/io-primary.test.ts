import { describe, expect, it } from "vitest";

import { primaryBlock, setPrimaryPad } from "@/lib/io-primary";
import { ApiError } from "@/shared/api-fetch";

describe("primary controller change", () => {
  it("is blocked while the arm state is armed or unknown", () => {
    expect(primaryBlock(true)).toBe("armed");
    expect(primaryBlock(undefined)).toBe("unknown");
    expect(primaryBlock(null)).toBe("unknown");
    expect(primaryBlock(false)).toBeNull();
  });

  it("asks before forcing the change when the agent refuses it as armed", async () => {
    const calls: unknown[] = [];
    const put = async (_path: string, init: { method: string; body: unknown }) => {
      calls.push(init.body);
      if (calls.length === 1) throw new ApiError("armed", 409, { error: "E_ARMED", override: "force" });
      return {};
    };
    let asked = 0;
    await setPrimaryPad("pad-1", async () => (asked++, true), put);
    expect(asked).toBe(1);
    expect(calls).toEqual([{ device_id: "pad-1" }, { device_id: "pad-1", force: true }]);
  });
});
