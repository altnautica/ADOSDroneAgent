import { beforeEach, describe, expect, it, vi } from "vitest";

const apiFetch = vi.fn();
vi.mock("../api-fetch", () => ({ apiFetch: (...args: unknown[]) => apiFetch(...args) }));

import { putConfigChecked } from "./apply-actions";

describe("putConfigChecked", () => {
  beforeEach(() => {
    apiFetch.mockReset();
    apiFetch.mockResolvedValue({ ok: true });
  });

  it("sends only key and value by default", async () => {
    await putConfigChecked("mavlink.baud_rate", "115200");
    expect(apiFetch).toHaveBeenCalledWith("/api/config", {
      method: "PUT",
      body: { key: "mavlink.baud_rate", value: "115200" },
    });
  });

  it("adds force when the override is asked for", async () => {
    await putConfigChecked("mavlink.baud_rate", "115200", { force: true });
    expect(apiFetch).toHaveBeenCalledWith("/api/config", {
      method: "PUT",
      body: { key: "mavlink.baud_rate", value: "115200", force: true },
    });
  });
});
