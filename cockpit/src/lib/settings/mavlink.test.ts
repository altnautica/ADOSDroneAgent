import { describe, expect, it, vi } from "vitest";

import { settleGate } from "@/lib/settings/apply-form";
import { stepLeafChange } from "@/lib/settings/guarded-leaf";
import {
  MAVLINK_KEYS,
  armedFrom,
  baudOptions,
  endpointValue,
  mavlinkBanner,
  toSource,
  transportFields,
  writeMavlinkLeaf,
} from "@/lib/settings/mavlink";
import type { putConfigChecked } from "@/shared/api/apply-actions";
import { ApiError } from "@/shared/api-fetch";

const armedRefusal = () => new ApiError("Vehicle is armed", 409, { error: "E_ARMED", override: "force" });

describe("mavlink transport fields", () => {
  it("enables the target and baud per source", () => {
    expect(transportFields("auto")).toEqual({ isNetwork: false, targetEnabled: false, baudEnabled: false });
    expect(transportFields("serial")).toEqual({ isNetwork: false, targetEnabled: true, baudEnabled: true });
    expect(transportFields("udp")).toEqual({ isNetwork: true, targetEnabled: true, baudEnabled: false });
    expect(transportFields("tcp")).toEqual({ isNetwork: true, targetEnabled: true, baudEnabled: false });
  });

  it("reads an unknown source as auto", () => {
    expect(toSource("serial")).toBe("serial");
    expect(toSource(undefined)).toBe("auto");
    expect(toSource("bogus")).toBe("auto");
  });

  it("offers the standard bauds plus a non-standard stored one", () => {
    expect(baudOptions(115200)).toContain("115200");
    const opts = baudOptions(250000);
    expect(opts).toContain("250000");
    expect(opts.map(Number)).toEqual([...opts.map(Number)].sort((a, b) => a - b));
    expect(baudOptions(300)).not.toContain("300");
  });

  it("formats an endpoint with the disabled suffix", () => {
    expect(endpointValue({ host: "10.0.0.2", port: 14550 }, " (off)")).toBe("10.0.0.2:14550");
    expect(endpointValue({ port: 5760, enabled: false }, " (off)")).toBe("0.0.0.0:5760 (off)");
  });
});

describe("mavlink armed handling", () => {
  it("reads the arm state from the live vehicle read", () => {
    expect(armedFrom({ armed: true }, false)).toBe(true);
    expect(armedFrom({}, false)).toBe(false);
    expect(armedFrom(null, false)).toBeNull();
    expect(armedFrom({ armed: false }, true)).toBeNull();
  });

  it("bans nothing: the arm state is only a banner", () => {
    expect(mavlinkBanner(false)).toBe("none");
    expect(mavlinkBanner(true)).toBe("armed");
    expect(mavlinkBanner(null)).toBe("unknown");
  });

  it("still holds every link change behind the guard", () => {
    const change = { key: MAVLINK_KEYS.baudRate, value: 115200 };
    const step = stepLeafChange(change, true);
    expect(step.kind).toBe("hold");
    if (step.kind === "hold") {
      expect(settleGate(step.gate, false)).toBeNull();
      expect(settleGate(step.gate, true)).toEqual(change);
    }
  });

  it("retries with force after the operator confirms an armed refusal", async () => {
    const put = vi
      .fn<typeof putConfigChecked>()
      .mockRejectedValueOnce(armedRefusal())
      .mockResolvedValueOnce({ ok: true });
    const confirm = vi.fn(async () => true);

    await expect(writeMavlinkLeaf(MAVLINK_KEYS.baudRate, "115200", confirm, put)).resolves.toEqual({ ok: true });
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(put.mock.calls).toEqual([
      [MAVLINK_KEYS.baudRate, "115200", {}],
      [MAVLINK_KEYS.baudRate, "115200", { force: true }],
    ]);
  });

  it("sends no forced write when the operator declines", async () => {
    const refusal = armedRefusal();
    const put = vi.fn<typeof putConfigChecked>().mockRejectedValue(refusal);

    await expect(writeMavlinkLeaf(MAVLINK_KEYS.source, "udp", async () => false, put)).rejects.toBe(refusal);
    expect(put.mock.calls).toEqual([[MAVLINK_KEYS.source, "udp", {}]]);
  });

  it("writes once without asking when the agent accepts", async () => {
    const put = vi.fn<typeof putConfigChecked>().mockResolvedValue({ ok: true });
    const confirm = vi.fn(async () => true);

    await writeMavlinkLeaf(MAVLINK_KEYS.systemId, "2", confirm, put);
    expect(confirm).not.toHaveBeenCalled();
    expect(put).toHaveBeenCalledTimes(1);
  });
});
