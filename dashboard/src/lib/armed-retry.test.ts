import { describe, expect, it, vi } from "vitest";

import { ApiError } from "@/shared/api-fetch";

import { ARMED_CONFIRM_MESSAGE, withArmedRetry } from "./armed-retry";

const armed = () =>
  new ApiError("Vehicle is armed", 409, {
    error: "E_ARMED",
    message: "Vehicle is armed",
    override: "force",
  });

describe("withArmedRetry", () => {
  it("retries with force after the operator confirms an armed refusal", async () => {
    const call = vi
      .fn<(force: boolean) => Promise<string>>()
      .mockRejectedValueOnce(armed())
      .mockResolvedValueOnce("applied");
    const confirm = vi.fn(async () => true);

    await expect(withArmedRetry(call, confirm)).resolves.toBe("applied");
    expect(confirm).toHaveBeenCalledWith(ARMED_CONFIRM_MESSAGE);
    expect(call.mock.calls).toEqual([[false], [true]]);
  });

  it("recognises the refusal nested under detail", async () => {
    const nested = new ApiError("conflict", 409, {
      detail: { error: "E_ARMED", message: "Vehicle is armed", override: "force" },
    });
    const call = vi
      .fn<(force: boolean) => Promise<string>>()
      .mockRejectedValueOnce(nested)
      .mockResolvedValueOnce("applied");

    await expect(withArmedRetry(call, async () => true)).resolves.toBe("applied");
    expect(call.mock.calls).toEqual([[false], [true]]);
  });

  it("rejects with the refusal when the operator declines", async () => {
    const refusal = armed();
    const call = vi.fn<(force: boolean) => Promise<string>>().mockRejectedValue(refusal);

    await expect(withArmedRetry(call, async () => false)).rejects.toBe(refusal);
    expect(call).toHaveBeenCalledTimes(1);
  });

  it("does not ask on any other error", async () => {
    const other = new ApiError("bad", 422, { detail: "bad" });
    const call = vi.fn<(force: boolean) => Promise<string>>().mockRejectedValue(other);
    const confirm = vi.fn(async () => true);

    await expect(withArmedRetry(call, confirm)).rejects.toBe(other);
    expect(confirm).not.toHaveBeenCalled();
  });
});
