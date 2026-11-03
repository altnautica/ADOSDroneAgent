import { beforeEach, describe, expect, it, vi } from "vitest";

// vi.mock calls below are hoisted above this import.
import { MspFcClient } from "@/shared/api/msp/fc-settings";

// The FC's reply to the armed-state read, per test: a status payload, or a
// thrown error (an FC that did not answer).
let statusReply: Uint8Array | Error = new Uint8Array(16);
const sent: Uint8Array[] = [];

vi.mock("@/shared/ws-ticket", () => ({
  WS_TICKET_PROTOCOL: "ados-ws-ticket",
  mintWsTicket: async () => null,
}));

vi.mock("@/shared/api/msp/transport", () => ({
  mavlinkWsUrl: () => "ws://test/",
  WebSocketTransport: class {
    async connect() {}
    on() {}
    off() {}
    send(bytes: Uint8Array) {
      sent.push(bytes);
    }
    async disconnect() {}
  },
}));

vi.mock("@/shared/api/msp/serial-queue", () => ({
  MspSerialQueue: class {
    async send() {
      if (statusReply instanceof Error) throw statusReply;
      return { command: 150, payload: statusReply };
    }
    destroy() {}
  },
}));

/** A Betaflight MSP_STATUS_EX payload with the ARM box set or clear. */
function statusEx(armed: boolean): Uint8Array {
  const p = new Uint8Array(16);
  p[6] = armed ? 1 : 0;
  return p;
}

async function open(): Promise<{ refusal: string | null; text: string[] }> {
  const client = new MspFcClient("betaflight");
  await client.connect();
  const text: string[] = [];
  const refusal = await client.openCli((t) => text.push(t));
  return { refusal, text };
}

describe("MspFcClient.openCli", () => {
  beforeEach(() => {
    sent.length = 0;
  });

  it("refuses an armed FC and sends nothing to the CLI", async () => {
    statusReply = statusEx(true);
    const { refusal } = await open();
    expect(refusal).toMatch(/armed/i);
    expect(sent).toHaveLength(0);
  });

  it("refuses when the armed state cannot be read", async () => {
    statusReply = new Error("timeout");
    const { refusal } = await open();
    expect(refusal).not.toBeNull();
    expect(sent).toHaveLength(0);
  });

  it("enters the CLI on a disarmed FC", async () => {
    statusReply = statusEx(false);
    const { refusal } = await open();
    expect(refusal).toBeNull();
    expect(new TextDecoder().decode(sent[0])).toBe("#\r\n");
  });
});
