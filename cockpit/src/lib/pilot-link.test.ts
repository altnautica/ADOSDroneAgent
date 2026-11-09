// The cockpit's pilot client frames against the agent's shared fixture
// (`crates/ados-control/tests/fixtures/pilot-requests.json`), which the pilot
// stream's own surface-frame decoder is tested with. A frame here that drifts
// from the fixture is one the agent would refuse or misread.

import { readFileSync } from "node:fs";
import path from "node:path";

import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";

import {
  connectPilotLink,
  releasePilotInput,
  sendPilotAction,
  sendPilotInput,
  takePilotSession,
  usePilotLinkStore,
} from "@/lib/pilot-link";

vi.mock("@/shared/ws-ticket", () => ({ WS_TICKET_PROTOCOL: "ticket", mintWsTicket: async () => null }));

const FIXTURE = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures/pilot-requests.json");
const shared = JSON.parse(readFileSync(FIXTURE, "utf8")) as Record<string, Record<string, unknown>>;

const sent: Record<string, unknown>[] = [];
class FakeSocket {
  static OPEN = 1;
  readyState = 1;
  onopen: (() => void) | null = null;
  onmessage: unknown = null;
  onclose: unknown = null;
  onerror: unknown = null;
  constructor() {
    queueMicrotask(() => this.onopen?.());
  }
  send(text: string): void {
    sent.push(JSON.parse(text));
  }
  close(): void {}
}

const link = { connectPilotLink, releasePilotInput, sendPilotAction, sendPilotInput, takePilotSession, usePilotLinkStore };
let disconnect: () => void;

/** The fixture frame as this surface would send it: its `src`, and its own
 *  values for the per-sample fields (session, seq, clock, action id). */
function expected(name: string, frame: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = { ...shared[name], src: "cockpit" };
  for (const k of ["session", "seq", "t_ms", "id"]) if (k in out) out[k] = frame[k];
  return out;
}

const last = () => sent[sent.length - 1];

beforeAll(async () => {
  vi.stubGlobal("window", {});
  vi.stubGlobal("location", { protocol: "http:", host: "drone.local" });
  vi.stubGlobal("WebSocket", FakeSocket);
  disconnect = link.connectPilotLink("cockpit");
  await vi.waitFor(() => expect(link.usePilotLinkStore.getState().link).toBe("connected"));
});

afterAll(() => {
  disconnect();
  vi.unstubAllGlobals();
});

describe("cockpit pilot frames match the agent fixture", () => {
  it("input", () => {
    const { axes, buttons } = shared.input as { axes: [number, number, number, number]; buttons: number };
    expect(link.sendPilotInput(axes, buttons)).toBe(true);
    const f = last();
    expect(typeof f.session).toBe("number");
    expect(typeof f.seq).toBe("number");
    expect(typeof f.t_ms).toBe("number");
    expect(f).toEqual(expected("input", f));
  });

  it("release", () => {
    link.releasePilotInput();
    expect(last()).toEqual(expected("release", last()));
  });

  it("session.take", () => {
    link.takePilotSession();
    expect(last()).toEqual(expected("session_take", last()));
  });

  it("action", () => {
    const a = shared.action as { name: string; args: Record<string, unknown> };
    void link.sendPilotAction(a.name, a.args);
    const f = last();
    expect(typeof f.id).toBe("string");
    expect(f).toEqual(expected("action", f));
  });

  it("hb", async () => {
    vi.useFakeTimers();
    try {
      const d = link.connectPilotLink("cockpit");
      await vi.advanceTimersByTimeAsync(5000);
      d();
    } finally {
      vi.useRealTimers();
    }
    const hb = sent.find((f) => f.op === "hb");
    expect(hb).toEqual(expected("hb", hb!));
  });
});
