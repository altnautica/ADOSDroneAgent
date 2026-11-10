import { describe, expect, it } from "vitest";

import { cameraLabel, feedCameras, pickPipCamera, resolveLegVideoUrls } from "@/lib/feed-legs";
import { useFeedStore } from "@/stores/feed-store";

// The Feed resolves the active leg's WHEP + HLS endpoints RELATIVE to whatever
// origin the operator reached the agent on — never an absolute host:port.
// WebRTC ICE will not traverse a remote/Tailscale hop, so the cockpit must be
// able to fall back to HLS and that HLS URL must resolve on the same origin the
// browser is already talking to.
describe("resolveLegVideoUrls", () => {
  it("defaults to the primary /whep + /hls/main endpoints for no camera", () => {
    expect(resolveLegVideoUrls(null)).toEqual({
      whepUrl: "/whep",
      hlsUrl: "/hls/main/index.m3u8",
    });
  });

  it("defaults a bare primary leg (id `main`) to the primary HLS endpoint", () => {
    expect(resolveLegVideoUrls({ id: "main" })).toEqual({
      whepUrl: "/whep",
      hlsUrl: "/hls/main/index.m3u8",
    });
  });

  it("builds per-leg WHEP + HLS endpoints from a secondary camera id", () => {
    expect(resolveLegVideoUrls({ id: "belly" })).toEqual({
      whepUrl: "/whep?camera=belly",
      hlsUrl: "/hls/belly/index.m3u8",
    });
  });

  it("offers the concurrent legs from the status body when there are several", () => {
    const cams = feedCameras(
      [
        { id: "main", whepUrl: "/whep?camera=main", hls: "/hls/main/index.m3u8" },
        { id: "ir", whepUrl: "/whep?camera=ir", hls: "/hls/ir/index.m3u8" },
      ],
      [{ id: "roster-only" }],
    );
    expect(cams.map((c) => [c.id, c.whep_url])).toEqual([
      ["main", "/whep?camera=main"],
      ["ir", "/whep?camera=ir"],
    ]);
    expect(feedCameras([{ id: "main" }], [{ id: "a" }, { id: "b" }]).map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("prefers explicit roster URLs when the agent advertises them", () => {
    expect(
      resolveLegVideoUrls({
        id: "belly",
        whep_url: "/whep?camera=belly",
        hls_url: "/hls/belly/index.m3u8",
      }),
    ).toEqual({
      whepUrl: "/whep?camera=belly",
      hlsUrl: "/hls/belly/index.m3u8",
    });
  });

  it("always stays relative / same-origin — never an absolute host:port", () => {
    const { whepUrl, hlsUrl } = resolveLegVideoUrls({ id: "ir" });
    expect(whepUrl.startsWith("http")).toBe(false);
    expect(hlsUrl.startsWith("http")).toBe(false);
    expect(whepUrl.startsWith("/")).toBe(true);
    expect(hlsUrl.startsWith("/")).toBe(true);
  });
});

describe("cameraLabel", () => {
  it("names a down-facing leg Belly", () => {
    expect(cameraLabel({ id: "belly", role: "down" })).toBe("Belly");
  });
  it("prefers the agent's label, then the role's label, then name, then id", () => {
    expect(cameraLabel({ id: "x", label: "Nose", role: "down" })).toBe("Nose");
    expect(cameraLabel({ id: "x", name: "primary", role: "primary" })).toBe("Primary");
    expect(cameraLabel({ id: "x", name: "/dev/video2", role: "down" })).toBe("Belly");
    expect(cameraLabel({ id: "x", name: "Cam 2" })).toBe("Cam 2");
    expect(cameraLabel({ id: "x" })).toBe("x");
  });
});

describe("pickPipCamera", () => {
  const main = { id: "main", role: "primary" };
  const belly = { id: "belly", role: "down" };
  const aux = { id: "aux" };

  it("shows no inset with a single leg", () => {
    expect(pickPipCamera([main], null, null)).toBeNull();
  });
  it("defaults the inset to the belly leg", () => {
    expect(pickPipCamera([main, aux, belly], null, null)?.id).toBe("belly");
    expect(pickPipCamera([main, aux, belly], "main", null)?.id).toBe("belly");
  });
  it("never shows the main leg in the inset", () => {
    expect(pickPipCamera([main, belly], "belly", "belly")?.id).toBe("main");
  });
  it("keeps the operator's previous main in the inset", () => {
    expect(pickPipCamera([main, aux, belly], "belly", "aux")?.id).toBe("aux");
  });
  it("falls back when the remembered leg is gone", () => {
    expect(pickPipCamera([main, belly], "main", "aux")?.id).toBe("belly");
  });
});

describe("feed store swapPip", () => {
  it("moves the inset leg to the main view and keeps the old main in the inset", () => {
    useFeedStore.getState().syncLegs("main", "belly");
    useFeedStore.getState().swapPip();
    const after = useFeedStore.getState();
    expect(after.activeCameraId).toBe("belly");
    expect(after.mainCameraId).toBe("belly");
    expect(after.pipCameraId).toBe("main");
    const cams = [{ id: "main" }, { id: "belly", role: "down" }];
    expect(pickPipCamera(cams, after.activeCameraId, after.pipCameraId)?.id).toBe("main");
  });
  it("does nothing without an inset", () => {
    useFeedStore.setState({ activeCameraId: "main", mainCameraId: "main", pipCameraId: null });
    useFeedStore.getState().swapPip();
    expect(useFeedStore.getState().activeCameraId).toBe("main");
  });
});
