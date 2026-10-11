import { beforeEach, describe, expect, it, vi } from "vitest";

import type * as Api from "@/lib/api";
import type { GsStatus } from "@/lib/types";

import { statusReader } from "./use-status-poll";

const api = vi.hoisted(() => ({
  getGsStatus: vi.fn(),
  getFcConnected: vi.fn(),
  getStatusFull: vi.fn(),
  getLinkDiagnosis: vi.fn(),
}));

// Hoisted above the imports, so `statusReader` binds to these doubles.
vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof Api>()),
  ...api,
}));

/** A ground-station composite as the agent sends it: no `fc_connected` key. */
function gsBody(): GsStatus {
  return { profile: "ground-station" } as GsStatus;
}

const signal = new AbortController().signal;

beforeEach(() => {
  vi.clearAllMocks();
  api.getGsStatus.mockResolvedValue(gsBody());
  api.getLinkDiagnosis.mockResolvedValue(null);
});

describe("statusReader on a ground station", () => {
  it("carries the node's FC link, which the composite does not report", async () => {
    // The skill gate reads `status.fc_connected`. A ground station whose router
    // holds a live MAVLink link reports it on /api/status, the same snapshot
    // field POST /api/command gates on; the composite has no such key.
    api.getFcConnected.mockResolvedValue(true);
    const status = await statusReader("ground_station")(signal);
    expect(status.fc_connected).toBe(true);
  });

  it("reads no link when the node has none", async () => {
    api.getFcConnected.mockResolvedValue(false);
    expect((await statusReader("ground_station")(signal)).fc_connected).toBe(false);
  });

  it("reads no link when the link read fails, rather than holding a guess", async () => {
    api.getFcConnected.mockRejectedValue(new Error("unreachable"));
    expect((await statusReader("ground_station")(signal)).fc_connected).toBe(false);
  });

  it("re-reads the link on the slower cadence, not every tick", async () => {
    api.getFcConnected.mockResolvedValue(true);
    const read = statusReader("ground_station");
    await read(signal);
    await read(signal);
    expect(api.getGsStatus).toHaveBeenCalledTimes(2);
    expect(api.getFcConnected).toHaveBeenCalledTimes(1);
  });

  it("still fails the tick when the composite itself fails", async () => {
    api.getFcConnected.mockResolvedValue(true);
    api.getGsStatus.mockRejectedValue(new Error("down"));
    await expect(statusReader("ground_station")(signal)).rejects.toThrow("down");
  });
});

describe("statusReader on a drone", () => {
  it("takes the FC link from the consolidated body and makes no extra read", async () => {
    api.getStatusFull.mockResolvedValue({ fc_connected: true });
    const status = await statusReader("drone")(signal);
    expect(status.fc_connected).toBe(true);
    expect(api.getFcConnected).not.toHaveBeenCalled();
    expect(api.getGsStatus).not.toHaveBeenCalled();
  });
});
