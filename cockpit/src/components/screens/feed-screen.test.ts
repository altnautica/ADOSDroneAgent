import { describe, expect, it } from "vitest";

import { resolveLegVideoUrls } from "@/components/screens/feed-screen";

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

  it("builds a per-leg HLS endpoint from a secondary camera id", () => {
    expect(resolveLegVideoUrls({ id: "belly" })).toEqual({
      whepUrl: "/whep",
      hlsUrl: "/hls/belly/index.m3u8",
    });
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
