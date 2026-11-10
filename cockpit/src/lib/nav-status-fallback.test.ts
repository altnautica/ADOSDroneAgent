// The nav-status fallback: after the stream has been down 5 s the Feed polls
// `GET /api/v1/nav/status` at 2 Hz, ingests every kind present stamped with
// its age on the drone, and reports the reduced rate until the stream is back.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { connectNavStream, NAV_STATUS_POLL_MS, NAV_STREAM_FALLBACK_MS, pollNavStatus, setViewLease } from "@/lib/nav-stream";
import { useNavStreamStore } from "@/stores/nav-stream-store";

vi.mock("@/shared/ws-ticket", () => ({ WS_TICKET_PROTOCOL: "ticket", mintWsTicket: async () => null }));

const PILOT = {
  owner: "pilot",
  armed: true,
  landed: false,
  control_mode: "ados",
};

const STATUS = {
  pilot: { ts_ms: 1, age_ms: 300, data: PILOT },
  follow: { ts_ms: 1, age_ms: 100, data: { phase: "following" } },
  nav: null,
  view_demand: {},
};

let urls: string[] = [];
let body: unknown = STATUS;

function stubFetch(): void {
  vi.stubGlobal("fetch", async (url: string) => {
    urls.push(url);
    return new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });
  });
}

/** A socket that opens or fails as the test says. */
class FakeSocket {
  static OPEN = 1;
  static last: FakeSocket | null = null;
  readyState = 0;
  onopen: (() => void) | null = null;
  onmessage: unknown = null;
  onclose: (() => void) | null = null;
  onerror: unknown = null;
  constructor() {
    FakeSocket.last = this;
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.();
  }
  send(): void {}
  close(): void {
    this.readyState = 3;
    this.onclose?.();
  }
}

beforeEach(() => {
  urls = [];
  body = STATUS;
  useNavStreamStore.getState().clear();
  setViewLease(null);
  stubFetch();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("pollNavStatus", () => {
  it("ingests every kind present, stamped with its age on the drone", async () => {
    expect(await pollNavStatus(undefined, () => 10_000)).toBe(true);
    const s = useNavStreamStore.getState();
    expect(s.pilot?.at).toBe(9_700);
    expect(s.pilot?.data.armed).toBe(true);
    expect(s.follow?.at).toBe(9_900);
    expect(s.nav).toBeNull();
    expect(urls).toEqual(["/api/v1/nav/status"]);
  });

  it("carries the view lease as the view query", async () => {
    setViewLease({ cameraId: "main", layers: ["ar", "depth"] });
    await pollNavStatus();
    expect(urls).toEqual([`/api/v1/nav/status?view=${encodeURIComponent("main:ar,depth")}`]);
  });

  it("resolves false on a failed read", async () => {
    vi.stubGlobal("fetch", async () => new Response("", { status: 502 }));
    expect(await pollNavStatus()).toBe(false);
  });
});

describe("stream fallback", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("window", {});
    vi.stubGlobal("location", { protocol: "http:", host: "drone" });
    vi.stubGlobal("document", { visibilityState: "visible" });
    vi.stubGlobal("WebSocket", FakeSocket);
  });

  const statusPolls = () => urls.filter((u) => u.startsWith("/api/v1/nav/status")).length;

  it("polls nav status at 2 Hz once the stream has been down 5 s, and stops when it returns", async () => {
    const stop = connectNavStream();
    await vi.advanceTimersByTimeAsync(NAV_STREAM_FALLBACK_MS - NAV_STATUS_POLL_MS);
    expect(statusPolls()).toBe(0);
    expect(useNavStreamStore.getState().radioFallback).toBe(false);

    await vi.advanceTimersByTimeAsync(NAV_STATUS_POLL_MS * 4);
    expect(statusPolls()).toBeGreaterThanOrEqual(3);
    expect(useNavStreamStore.getState().radioFallback).toBe(true);
    expect(useNavStreamStore.getState().pilot?.data.armed).toBe(true);

    // The stream comes back: the poll stops and the reduced rate clears.
    await vi.advanceTimersByTimeAsync(20_000);
    FakeSocket.last!.open();
    expect(useNavStreamStore.getState().radioFallback).toBe(false);
    const n = statusPolls();
    await vi.advanceTimersByTimeAsync(NAV_STREAM_FALLBACK_MS * 2);
    expect(statusPolls()).toBe(n);
    stop();
  });

  it("does not report the reduced rate while the status read fails too", async () => {
    vi.stubGlobal("fetch", async (url: string) => {
      urls.push(url);
      return new Response("", { status: 502 });
    });
    const stop = connectNavStream();
    await vi.advanceTimersByTimeAsync(NAV_STREAM_FALLBACK_MS + NAV_STATUS_POLL_MS * 3);
    expect(statusPolls()).toBeGreaterThan(0);
    expect(useNavStreamStore.getState().radioFallback).toBe(false);
    stop();
  });
});
