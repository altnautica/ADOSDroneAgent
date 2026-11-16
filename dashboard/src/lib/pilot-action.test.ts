import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ACTION_TIMEOUT_MS, PILOT_HB_MS, PilotActionLink, REDIAL_MS, type SocketLike } from "@/lib/pilot-action";

class FakeSocket implements SocketLike {
  readyState = 0;
  sent: Record<string, unknown>[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(
    readonly url: string,
    readonly protocols: string[],
  ) {}

  send(data: string): void {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }
  close(): void {
    this.readyState = 3;
    this.onclose?.();
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.();
  }
  answer(id: unknown, ok: boolean, err?: string): void {
    this.onmessage?.({ data: JSON.stringify({ kind: "action_result", name: "x", id, ok, err }) });
  }
  actions(): Record<string, unknown>[] {
    return this.sent.filter((f) => f.op === "action");
  }
}

let sockets: FakeSocket[];
let link: PilotActionLink;

function newLink(ticket: string | null = "t1"): PilotActionLink {
  return new PilotActionLink({
    dial: (url, protocols) => {
      const s = new FakeSocket(url, protocols);
      sockets.push(s);
      return s;
    },
    ticket: () => Promise.resolve(ticket),
    url: () => "ws://node/api/v1/pilot/stream",
  });
}

/** Let the ticket promise and the dial run. */
const settle = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
  sockets = [];
  link = newLink();
});

afterEach(() => {
  link.close();
  vi.useRealTimers();
});

describe("the dashboard's pilot actions", () => {
  it("send each stall and countdown action on the pilot stream, answered by id", async () => {
    const results: Promise<unknown>[] = [];
    results.push(link.send("mission_skip"));
    await settle();
    expect(sockets).toHaveLength(1);
    expect(sockets[0].url).toBe("ws://node/api/v1/pilot/stream");
    expect(sockets[0].protocols).toEqual(["ados-ws-ticket", "t1"]);
    sockets[0].open();
    await settle();
    for (const name of ["rth", "land", "rth_cancel"]) {
      results.push(link.send(name));
      await settle();
    }
    const frames = sockets[0].actions();
    expect(frames.map((f) => f.name)).toEqual(["mission_skip", "rth", "land", "rth_cancel"]);
    expect(frames.every((f) => f.src === "dashboard" && typeof f.id === "string")).toBe(true);
    expect(new Set(frames.map((f) => f.id)).size).toBe(4);
    sockets[0].answer(frames[0].id, true);
    sockets[0].answer(frames[1].id, false, "busy");
    sockets[0].answer(frames[2].id, true);
    sockets[0].answer(frames[3].id, true);
    expect(await Promise.all(results)).toEqual([{ ok: true }, { ok: false, err: "busy" }, { ok: true }, { ok: true }]);
    // One link for every action.
    expect(sockets).toHaveLength(1);
  });

  it("dials bare without a ticket", async () => {
    link = newLink(null);
    void link.send("land");
    await settle();
    expect(sockets[0].protocols).toEqual([]);
  });

  it("reports a timeout when the pilot never answers", async () => {
    const r = link.send("land");
    await settle();
    sockets[0].open();
    await vi.advanceTimersByTimeAsync(ACTION_TIMEOUT_MS + 1);
    expect(await r).toEqual({ ok: false, err: "no response" });
  });

  it("reports no link when the stream never opens", async () => {
    const r = link.send("rth");
    await vi.advanceTimersByTimeAsync(ACTION_TIMEOUT_MS + 1);
    expect(await r).toEqual({ ok: false, err: "no pilot link" });
  });

  it("keeps answering the pilot's link-loss watch after acting, and redials a dropped link", async () => {
    void link.send("mission_skip");
    await settle();
    sockets[0].open();
    await vi.advanceTimersByTimeAsync(PILOT_HB_MS * 3);
    expect(sockets[0].sent.filter((f) => f.op === "hb")).toHaveLength(3);
    expect(sockets[0].sent.find((f) => f.op === "hb")).toEqual({ op: "hb", src: "dashboard", sid: link.sid });
    sockets[0].close();
    await vi.advanceTimersByTimeAsync(REDIAL_MS);
    expect(sockets).toHaveLength(2);
    sockets[1].open();
    await vi.advanceTimersByTimeAsync(PILOT_HB_MS);
    // The redialled link keeps the page's instance id.
    expect(sockets[1].sent).toEqual([{ op: "hb", src: "dashboard", sid: link.sid }]);
  });

  it("fails a pending action when the link drops", async () => {
    const r = link.send("rth");
    await settle();
    sockets[0].open();
    await settle();
    sockets[0].close();
    expect(await r).toEqual({ ok: false, err: "no pilot link" });
  });

  it("tags every frame with one instance id and says bye on close", async () => {
    void link.send("land");
    await settle();
    sockets[0].open();
    await vi.advanceTimersByTimeAsync(PILOT_HB_MS);
    expect(link.sid).toMatch(/^[0-9a-f]{32}$/);
    expect(newLink().sid).not.toBe(link.sid);
    link.close();
    expect(sockets[0].sent.map((f) => f.op)).toEqual(["action", "hb", "bye"]);
    expect(sockets[0].sent.every((f) => f.sid === link.sid && f.src === "dashboard")).toBe(true);
  });
});
