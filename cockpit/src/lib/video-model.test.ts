import { describe, expect, it } from "vitest";

import { armedRefusalText, radioWriteBlock } from "@/lib/radio-gate";
import { ApiError } from "@/shared/api-fetch";
import { draftPatch, fecRedundancyPct, parseCameras, parseLatency, parseVideoConfig, tuneBlock } from "@/lib/video-model";

const CFG = parseVideoConfig({
  encoder: { codec: "h264", width: 1280, height: 720, fps: 30, bitrate_kbps: 4000 },
  radio: { channel: 149, band: "5G", mcs_index: 2, fec_k: 8, fec_n: 12, tx_power_dbm: 12, preset: "balanced" },
  adaptive: { available: true, adaptive_bitrate_enabled: false },
});

describe("video reads", () => {
  it("reads the encoder, radio and adaptive switch", () => {
    expect(CFG?.radio).toEqual({ channel: 149, band: "5G", mcs: 2, fecK: 8, fecN: 12, txPowerDbm: 12, preset: "balanced" });
    expect(CFG?.adaptive).toBe(false);
    expect(parseVideoConfig({})?.adaptive).toBeNull();
    expect(parseVideoConfig(null)).toBeNull();
  });

  it("keeps only cameras with a device path and leaves missing latency null", () => {
    expect(parseCameras({ cameras: [{ device_path: "/dev/video0", type: "usb", label: "Cam" }, { label: "x" }] })).toHaveLength(1);
    expect(parseLatency({ samples: 3 })).toEqual({ latestMs: null, ewmaMs: null, samples: 3, source: null });
  });

  it("computes FEC redundancy and refuses an unusable pair", () => {
    expect(fecRedundancyPct(8, 12)).toBe(50);
    expect(fecRedundancyPct(8, 4)).toBeNull();
    expect(fecRedundancyPct(null, 12)).toBeNull();
  });
});

describe("link tuning draft", () => {
  it("sends only what differs from the live config", () => {
    expect(draftPatch({ preset: "balanced", mcs: 2 }, CFG)).toBeNull();
    expect(draftPatch({ mcs: 4, fec: [8, 16] }, CFG)).toEqual({ mcs: 4, fec_k: 8, fec_n: 16 });
    expect(draftPatch({ auto: true }, CFG)).toEqual({ auto: true });
  });
});

describe("link tuning confirm gate", () => {
  const patch = { mcs: 4 };
  it("applies only when disarmed with a change staged", () => {
    expect(tuneBlock(patch, radioWriteBlock({ connected: true, armed: false }))).toBeNull();
    expect(tuneBlock(null, radioWriteBlock({ connected: true, armed: false }))).toBe("noChange");
  });

  it("refuses while armed, and while the arm state is unknown", () => {
    expect(tuneBlock(patch, radioWriteBlock({ connected: true, armed: true }))).toBe("armed");
    expect(tuneBlock(patch, radioWriteBlock({ connected: true, armed: null }))).toBe("armedUnknown");
    expect(tuneBlock(patch, radioWriteBlock(null))).toBe("armedUnknown");
  });

  it("lets a node with no flight controller link write", () => {
    expect(radioWriteBlock({ connected: false, armed: null })).toBeNull();
    // A stale arm flag still refuses even with the link down.
    expect(radioWriteBlock({ connected: false, armed: true })).toBe("armed");
  });
});

describe("agent armed refusal", () => {
  it("shows the agent's own refusal text and ignores other failures", () => {
    const refusal = new ApiError("409", 409, { error: "E_ARMED", message: "Vehicle is armed", override: "force" });
    expect(armedRefusalText(refusal)).toBe("Vehicle is armed");
    expect(armedRefusalText(new ApiError("409", 409, { error: "E_CONFLICT" }))).toBeNull();
    expect(armedRefusalText(new Error("offline"))).toBeNull();
  });
});
