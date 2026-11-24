import { describe, expect, it } from "vitest";
import { clockText } from "@/components/shell/wall-clock";
import { linkStateKey, linkTone, regionLabelKey } from "./link-state";
import { videoReceiveView } from "@/shared/api/video-receive";

describe("link state mapping", () => {
  it("renders the receiver's active state as connected", () => {
    expect(linkTone("active", true)).toBe("ok");
    expect(linkStateKey("active")).toBe("cockpit.link.state.connected");
  });

  it("maps every radio daemon state to a label, never unknown", () => {
    for (const s of ["searching", "binding", "auto_pairing", "unpaired", "connecting", "disconnected"]) {
      expect(linkStateKey(s)).not.toBe("cockpit.link.state.unknown");
    }
    expect(linkTone("searching", false)).toBe("muted");
    expect(linkStateKey("bogus")).toBe("cockpit.link.state.unknown");
  });

  it("names the global regulatory domain Unrestricted", () => {
    expect(regionLabelKey("global")).toBe("cockpit.link.regionUnrestricted");
    expect(regionLabelKey("US")).toBeNull();
    expect(regionLabelKey(null)).toBeNull();
  });
});

describe("video receive view", () => {
  it("reads a served stream with viewers and SEI latency", () => {
    const v = videoReceiveView(
      { state: "running", mediamtx: { running: true, ready: true, tracks: ["H264"], readers: 2 } },
      { latency_ms: 142.4 },
    );
    expect(v.stateKey).toBe("cockpit.link.video.receiving");
    expect(v.viewers).toBe("2");
    expect(v.tracks).toBe("H264");
    expect(v.latency).toBe("142 ms");
  });

  it("says no stream when the media server has nothing", () => {
    const v = videoReceiveView({ state: "not_initialized", mediamtx: { running: true } }, { latency_ms: null });
    expect(v.stateKey).toBe("cockpit.link.video.noStream");
    expect(v.latency).toBe("—");
    expect(videoReceiveView(null, null).stateKey).toBe("cockpit.link.video.unknown");
  });
});

describe("wall clock", () => {
  it("names its zone", () => {
    expect(clockText(new Date(2026, 10, 24, 17, 5))).toMatch(/^17:05 \S+/);
  });
});
