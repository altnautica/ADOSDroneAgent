import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../ws-ticket", () => ({
  mintWsTicket: vi.fn(async () => "ticket"),
  ticketProtocols: (t: string) => [t],
}));

import { openSetupStream } from "./client";

// A socket on a silently dead link: `close()` starts the closing handshake,
// and the browser fires `close` only once that handshake times out, so the
// fake never fires `onclose` on its own. `finishClose()` stands in for that
// late event.
class FakeSocket {
  static last: FakeSocket | null = null;
  static count = 0;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  closed = false;
  constructor() {
    FakeSocket.last = this;
    FakeSocket.count += 1;
  }
  close() {
    this.closed = true;
  }
  finishClose() {
    this.onclose?.();
  }
}

describe("setup stream liveness", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("window", { setTimeout, clearTimeout, location: { protocol: "http:", host: "node" } });
    vi.stubGlobal("WebSocket", FakeSocket);
    FakeSocket.last = null;
    FakeSocket.count = 0;
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

  it("reports down and redials on silence without waiting for the close event", async () => {
    const ups: boolean[] = [];
    const stop = openSetupStream(() => undefined, undefined, (up) => ups.push(up));
    await vi.advanceTimersByTimeAsync(0);
    const first = FakeSocket.last!;
    first.onopen!();
    await vi.advanceTimersByTimeAsync(5000);
    expect(ups).toEqual([true, false]);
    expect(first.closed).toBe(true);
    // The redial runs on the backoff timer even though `close` never arrived.
    await vi.advanceTimersByTimeAsync(1000);
    expect(FakeSocket.count).toBe(2);
    const second = FakeSocket.last!;
    expect(second).not.toBe(first);
    second.onopen!();
    expect(ups).toEqual([true, false, true]);
    // The dead socket's late close event changes nothing.
    first.finishClose();
    await vi.advanceTimersByTimeAsync(2000);
    expect(ups).toEqual([true, false, true]);
    expect(FakeSocket.count).toBe(2);
    stop();
  });

  it("reports down and redials when the server closes the socket", async () => {
    const ups: boolean[] = [];
    const stop = openSetupStream(() => undefined, undefined, (up) => ups.push(up));
    await vi.advanceTimersByTimeAsync(0);
    FakeSocket.last!.onopen!();
    FakeSocket.last!.finishClose();
    expect(ups).toEqual([true, false]);
    await vi.advanceTimersByTimeAsync(1000);
    expect(FakeSocket.count).toBe(2);
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
