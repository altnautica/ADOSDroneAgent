// A node with no pilot accepts the stream, answers one error frame and closes.
// The link must back off from that instead of redialling every second forever.

import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { connectPilotLink } from "@/lib/pilot-link";

vi.mock("@/shared/ws-ticket", () => ({ WS_TICKET_PROTOCOL: "ticket", mintWsTicket: async () => null }));

let dials = 0;
class RefusingSocket {
  static OPEN = 1;
  readyState = 1;
  onopen: (() => void) | null = null;
  onmessage: ((m: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: unknown = null;
  constructor() {
    dials += 1;
    queueMicrotask(() => {
      this.onopen?.();
      this.onmessage?.({ data: JSON.stringify({ kind: "error", err: "pilot_unavailable" }) });
      this.onclose?.();
    });
  }
  send(): void {}
  close(): void {}
}

beforeEach(() => {
  dials = 0;
  vi.useFakeTimers();
  vi.stubGlobal("window", { addEventListener: () => {}, removeEventListener: () => {} });
  vi.stubGlobal("location", { protocol: "http:", host: "drone.local" });
  vi.stubGlobal("WebSocket", RefusingSocket);
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("backs off from a node that answers pilot_unavailable and closes", async () => {
  const disconnect = connectPilotLink("kiosk");
  await vi.advanceTimersByTimeAsync(30_000);
  disconnect();
  // 1, 2, 4, 8, 10, 10 s after the first dial: 6 dials in 30 s, not 30.
  expect(dials).toBeLessThanOrEqual(7);
  expect(dials).toBeGreaterThanOrEqual(5);
});
