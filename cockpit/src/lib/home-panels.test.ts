import { describe, expect, it } from "vitest";

import { cloudHeartbeat, fcView, pushSample, sparkPoints, videoIdleReason, type CloudLink } from "@/lib/home-panels";
import type { FcSnapshot } from "@/shared/api/types";

describe("sparkline ring", () => {
  it("keeps one sample per second and bounds the ring", () => {
    let r = pushSample([], 1, 10, 3);
    r = pushSample(r, 1, 11, 3);
    expect(r).toEqual([{ time: 1, value: 11 }]);
    for (let s = 2; s <= 5; s += 1) r = pushSample(r, s, s, 3);
    expect(r.map((p) => p.time)).toEqual([3, 4, 5]);
  });

  it("treats a missing reading as a gap, not a zero", () => {
    const r = pushSample([{ time: 1, value: 4 }], 2, null);
    expect(r).toEqual([{ time: 1, value: 4 }]);
    expect(pushSample(r, 3, Number.NaN)).toBe(r);
  });

  it("draws nothing for one point and a mid line for a flat series", () => {
    expect(sparkPoints([{ time: 1, value: 1 }], 100, 20)).toBe("");
    expect(sparkPoints([{ time: 1, value: 5 }, { time: 2, value: 5 }], 100, 20)).toBe("0.0,10.0 100.0,10.0");
    expect(sparkPoints([{ time: 1, value: 0 }, { time: 2, value: 10 }], 100, 20)).toBe("0.0,20.0 100.0,0.0");
  });
});

describe("FC card state", () => {
  const fc = (connected: boolean, hb: string | null = "t") => ({ connected, last_heartbeat: hb }) as FcSnapshot;

  it("waits for the first snapshot", () => {
    expect(fcView(undefined, false, null)).toEqual({ kind: "connecting" });
  });

  it("shows an MSP FC by identity, not as missing", () => {
    expect(fcView(fc(false), true, "betaflight")).toEqual({ kind: "msp", variant: "betaflight" });
    expect(fcView(fc(false), true, null)).toEqual({ kind: "absent" });
  });

  it("flags a MAVLink FC with no heartbeat yet", () => {
    expect(fcView(fc(true, null), true, null)).toEqual({ kind: "mavlink", waiting: true });
    expect(fcView(fc(true), true, null)).toEqual({ kind: "mavlink", waiting: false });
  });
});

describe("cloud status post", () => {
  const link = (p: Partial<CloudLink>): CloudLink => ({
    paired: true,
    cloud_url_set: true,
    broker_connected: true,
    last_heartbeat_ok_ms: null,
    last_heartbeat_status: null,
    last_heartbeat_error: null,
    ...p,
  });

  it("prefers the age of the last good post", () => {
    expect(cloudHeartbeat(link({ last_heartbeat_ok_ms: 1000, last_heartbeat_status: 500 }), 6400)).toEqual({ key: "heartbeatOk", age: 5 });
  });

  it("falls back to the HTTP status, then the transport error, then not sent", () => {
    expect(cloudHeartbeat(link({ last_heartbeat_status: 503 }), 0)).toEqual({ key: "heartbeatHttp", status: 503 });
    expect(cloudHeartbeat(link({ last_heartbeat_error: "timeout" }), 0)).toEqual({ key: "noAnswer" });
    expect(cloudHeartbeat(link({}), 0)).toEqual({ key: "notSent" });
  });
});

describe("video idle reason", () => {
  const base = { pipelineState: "running", isGround: false, wfbPackets: null, haveStreamUrl: true };

  it("has none while the pipeline runs with a stream", () => {
    expect(videoIdleReason(base)).toBeNull();
  });

  it("waits on the drone's WFB stream on a ground station", () => {
    expect(videoIdleReason({ ...base, isGround: true, wfbPackets: 0 })).toBe("waitingForWfb");
    expect(videoIdleReason({ ...base, isGround: true, wfbPackets: 12 })).toBeNull();
  });

  it("names a missing camera or stream", () => {
    expect(videoIdleReason({ ...base, pipelineState: "no_camera" })).toBe("noCamera");
    expect(videoIdleReason({ ...base, haveStreamUrl: false })).toBe("noStream");
    expect(videoIdleReason({ ...base, pipelineState: "ready" })).toBe("pipelineIdle");
  });
});
