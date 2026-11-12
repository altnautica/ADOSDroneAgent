import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../ws-ticket", () => ({
  mintWsTicket: vi.fn(async () => "ticket"),
  ticketProtocols: (t: string) => [t],
}));

import { openSetupStream } from "./client";

class FakeSocket {
  static last: FakeSocket | null = null;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  constructor() {
    FakeSocket.last = this;
  }
  close() {
    this.onclose?.();
  }
}

describe("setup stream liveness", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("window", { setTimeout, clearTimeout, location: { protocol: "http:", host: "node" } });
    vi.stubGlobal("WebSocket", FakeSocket);
    FakeSocket.last = null;
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("reports up on open and down after 5 s of silence", async () => {
    const ups: boolean[] = [];
    const stop = openSetupStream(() => undefined, undefined, (up) => ups.push(up));
    await vi.advanceTimersByTimeAsync(0);
    FakeSocket.last!.onopen!();
    expect(ups).toEqual([true]);
    await vi.advanceTimersByTimeAsync(4000);
    FakeSocket.last!.onmessage!({ data: '{"kind":"keepalive"}' });
    await vi.advanceTimersByTimeAsync(4000);
    expect(ups).toEqual([true]);
    await vi.advanceTimersByTimeAsync(1500);
    expect(ups).toEqual([true, false]);
    stop();
  });

  it("reports down when no socket opens within 5 s", async () => {
    const ups: boolean[] = [];
    const stop = openSetupStream(() => undefined, undefined, (up) => ups.push(up));
    await vi.advanceTimersByTimeAsync(5100);
    expect(ups).toEqual([false]);
    stop();
  });

  it("stays quiet once stopped", async () => {
    const ups: boolean[] = [];
    const stop = openSetupStream(() => undefined, undefined, (up) => ups.push(up));
    await vi.advanceTimersByTimeAsync(0);
    FakeSocket.last!.onopen!();
    stop();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(ups).toEqual([true]);
  });
});
