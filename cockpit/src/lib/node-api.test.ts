import { describe, expect, it } from "vitest";

import { isJobActive, parseNodeJobs, parseNodeOutputs, parseNodeStatus, parseOffloadSessions } from "@/lib/node-api";

describe("node status", () => {
  it("reads the heartbeat and the offload session counts", () => {
    const s = parseNodeStatus({
      role: "master",
      cluster: { master_id: "n1", slaves: [{ node_id: "n2" }], aggregate_workers_idle: 3 },
      queue_depth: 2,
      active_jobs: 1,
      workers_idle: 1,
      active_sessions: 2,
      session_states: { opening: 0, live: 2, stalled: 0 },
    });
    expect(s).toEqual({
      role: "master",
      queueDepth: 2,
      activeJobs: 1,
      workersIdle: 1,
      activeSessions: 2,
      clusterSlaves: 1,
      sessionStates: { opening: 0, live: 2, stalled: 0 },
    });
  });

  it("leaves what the node did not send null", () => {
    expect(parseNodeStatus({})?.queueDepth).toBeNull();
    expect(parseNodeStatus(null)).toBeNull();
  });
});

describe("jobs, products and sessions", () => {
  it("keeps only well-formed jobs and reads either error shape", () => {
    const jobs = parseNodeJobs([
      { id: "j1", kind: "reconstruct", state: "running", progress: 0.4 },
      { id: "j2", kind: "reconstruct", state: "failed", error: { code: "E_TOOL_MISSING", message: "colmap missing" } },
      { id: "j3", state: "failed", error: "out of memory" },
      { kind: "no id" },
    ]);
    expect(jobs.map((j) => j.id)).toEqual(["j1", "j2", "j3"]);
    expect(jobs[0].progress).toBe(0.4);
    expect(jobs[1].error).toBe("colmap missing");
    expect(jobs[2].error).toBe("out of memory");
  });

  it("reads job outputs", () => {
    expect(parseNodeOutputs([{ id: "o1", kind: "splat", format: "spz", size_bytes: 10 }])).toEqual([
      { id: "o1", kind: "splat", format: "spz", file: null, sizeBytes: 10 },
    ]);
  });

  it("reads the live offload sessions", () => {
    const [s] = parseOffloadSessions([{ id: "s1", camera_id: "main", state: "live", frames_processed: 9, uptime_ms: 100 }]);
    expect(s).toMatchObject({ id: "s1", cameraId: "main", state: "live", framesProcessed: 9, uptimeMs: 100 });
  });

  it("treats only queued and running jobs as cancellable", () => {
    expect(isJobActive("queued")).toBe(true);
    expect(isJobActive("running")).toBe(true);
    expect(isJobActive("completed")).toBe(false);
  });

  it("returns nothing for a body that is not a list", () => {
    expect(parseNodeJobs({ detail: "not found" })).toEqual([]);
  });
});
