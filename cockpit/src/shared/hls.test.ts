import { describe, expect, it, vi } from "vitest";

import { startHls, type HlsCtor, type HlsErrorData } from "./hls";

function fakeVideo(): HTMLVideoElement {
  return {
    canPlayType: () => "",
    removeAttribute: () => undefined,
    play: () => Promise.resolve(),
  } as unknown as HTMLVideoElement;
}

/** A stand-in hls.js constructor recording every player it builds. */
function fakeHls() {
  const players: {
    handlers: Record<string, (evt: string, data: HlsErrorData) => void>;
    startLoad: ReturnType<typeof vi.fn>;
    destroy: ReturnType<typeof vi.fn>;
    recoverMediaError: ReturnType<typeof vi.fn>;
  }[] = [];
  class Player {
    handlers: Record<string, (evt: string, data: HlsErrorData) => void> = {};
    startLoad = vi.fn();
    destroy = vi.fn();
    recoverMediaError = vi.fn();
    constructor() {
      players.push(this);
    }
    on(event: string, cb: (evt: string, data: HlsErrorData) => void) {
      this.handlers[event] = cb;
    }
    attachMedia() {}
    loadSource() {}
    static isSupported() {
      return true;
    }
    static Events = { MANIFEST_PARSED: "parsed", ERROR: "error", FRAG_LOADED: "frag" };
    static ErrorTypes = { NETWORK_ERROR: "net", MEDIA_ERROR: "media" };
  }
  return { Ctor: Player as unknown as HlsCtor, players };
}

const netError: HlsErrorData = { fatal: true, type: "net", details: "fragLoadError" };

describe("startHls", () => {
  it("reports an established session lost after three consecutive fatal network errors", async () => {
    const { Ctor, players } = fakeHls();
    const onLost = vi.fn();
    const pending = startHls("/hls/main/index.m3u8", fakeVideo(), onLost, async () => Ctor);
    await Promise.resolve();
    await Promise.resolve();
    const p = players[0];
    p.handlers.parsed("parsed", {});
    expect((await pending).ok).toBe(true);

    p.handlers.error("error", netError);
    p.handlers.error("error", netError);
    expect(p.startLoad).toHaveBeenCalledTimes(2);
    expect(onLost).not.toHaveBeenCalled();

    p.handlers.error("error", netError);
    expect(onLost).toHaveBeenCalledTimes(1);
    expect(p.destroy).toHaveBeenCalledTimes(1);

    // A dead player stays dead: no further recovery or second report.
    p.handlers.error("error", netError);
    expect(onLost).toHaveBeenCalledTimes(1);
    expect(p.startLoad).toHaveBeenCalledTimes(2);
  });

  it("resets the error count when a segment loads", async () => {
    const { Ctor, players } = fakeHls();
    const onLost = vi.fn();
    const pending = startHls("/hls/main/index.m3u8", fakeVideo(), onLost, async () => Ctor);
    await Promise.resolve();
    await Promise.resolve();
    const p = players[0];
    p.handlers.parsed("parsed", {});
    await pending;
    p.handlers.error("error", netError);
    p.handlers.error("error", netError);
    p.handlers.frag("frag", {});
    p.handlers.error("error", netError);
    p.handlers.error("error", netError);
    expect(onLost).not.toHaveBeenCalled();
  });

  it("fails the start when the errors arrive before the manifest", async () => {
    const { Ctor, players } = fakeHls();
    const onLost = vi.fn();
    const pending = startHls("/hls/main/index.m3u8", fakeVideo(), onLost, async () => Ctor);
    await Promise.resolve();
    await Promise.resolve();
    const p = players[0];
    for (let i = 0; i < 3; i += 1) p.handlers.error("error", netError);
    const res = await pending;
    expect(res.ok).toBe(false);
    expect(onLost).not.toHaveBeenCalled();
  });

  it("keeps one player per element", async () => {
    const { Ctor, players } = fakeHls();
    const video = fakeVideo();
    void startHls("/hls/a/index.m3u8", video, undefined, async () => Ctor);
    await Promise.resolve();
    await Promise.resolve();
    void startHls("/hls/b/index.m3u8", video, undefined, async () => Ctor);
    await Promise.resolve();
    await Promise.resolve();
    expect(players).toHaveLength(2);
    expect(players[0].destroy).toHaveBeenCalledTimes(1);
    expect(players[1].destroy).not.toHaveBeenCalled();
  });
});
