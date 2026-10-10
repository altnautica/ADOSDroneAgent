import { describe, expect, it } from "vitest";

import { ReconnectBackoff, STABLE_SESSION_MS } from "@/lib/reconnect-backoff";

function clock() {
  let t = 0;
  return { now: () => t, advance: (ms: number) => (t += ms) };
}

describe("ReconnectBackoff", () => {
  it("keeps growing when the server accepts and closes at once", () => {
    const c = clock();
    const b = new ReconnectBackoff(1000, 10_000, c.now);
    const delays: number[] = [];
    for (let i = 0; i < 6; i += 1) {
      b.opened();
      c.advance(50);
      delays.push(b.next());
    }
    expect(delays).toEqual([1000, 2000, 4000, 8000, 10_000, 10_000]);
  });

  it("resets after a session that delivered a message", () => {
    const b = new ReconnectBackoff(1000, 10_000, clock().now);
    b.next();
    b.next();
    b.opened();
    b.message();
    expect(b.next()).toBe(1000);
  });

  it("resets after a session that stayed open long enough", () => {
    const c = clock();
    const b = new ReconnectBackoff(1000, 10_000, c.now);
    b.next();
    b.next();
    b.opened();
    c.advance(STABLE_SESSION_MS);
    expect(b.next()).toBe(1000);
  });

  it("grows across failed dials that never opened", () => {
    const b = new ReconnectBackoff(1000, 10_000, clock().now);
    expect([b.next(), b.next(), b.next()]).toEqual([1000, 2000, 4000]);
  });
});
