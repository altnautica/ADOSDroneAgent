import { describe, expect, it } from "vitest";

import {
  appendToTail,
  EMPTY_TAIL,
  logsPollPath,
  logsQuery,
  parseLogsResponse,
  parseSseChunk,
  visibleWindow,
} from "@/lib/logs-stream";

const entry = (seq: number, message = `m${seq}`) => JSON.stringify({ seq, timestamp: 1, level: "INFO", logger: "x", message });

describe("logs query", () => {
  it("adds only the filters set", () => {
    expect(logsQuery("", "")).toBe("");
    expect(logsQuery("ERROR", " ados.video ")).toBe("level=ERROR&service=ados.video");
    expect(logsPollPath("", "")).toBe("/api/logs?limit=200");
    expect(logsPollPath("INFO", "")).toBe("/api/logs?limit=200&level=INFO");
  });
});

describe("SSE parsing", () => {
  it("returns complete events and keeps the remainder", () => {
    const { events, rest } = parseSseChunk(`: keep-alive\n\ndata: {"a":1}\n\ndata: {"b"`);
    expect(events).toEqual(['{"a":1}']);
    expect(rest).toBe('data: {"b"');
  });

  it("joins multi-line data and accepts CRLF", () => {
    expect(parseSseChunk("data: one\r\ndata:two\r\n\r\n").events).toEqual(["one\ntwo"]);
  });
});

describe("live tail", () => {
  it("drops replayed seqs and malformed frames", () => {
    let tail = appendToTail(EMPTY_TAIL, [entry(1), entry(2), "not json", JSON.stringify({ seq: 9 })]);
    expect(tail.entries.map((e) => e.seq)).toEqual([1, 2]);
    tail = appendToTail(tail, [entry(1), entry(2), entry(3)]);
    expect(tail.entries.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(tail.lastSeq).toBe(3);
  });

  it("keeps only the newest entries at the cap", () => {
    const tail = appendToTail(EMPTY_TAIL, [1, 2, 3, 4, 5].map((n) => entry(n)), 3);
    expect(tail.entries.map((e) => e.seq)).toEqual([3, 4, 5]);
  });

  it("returns the same tail when nothing new arrived", () => {
    const tail = appendToTail(EMPTY_TAIL, [entry(1)]);
    expect(appendToTail(tail, [entry(1)])).toBe(tail);
  });
});

describe("poll body", () => {
  it("keeps the store warning and valid entries", () => {
    const r = parseLogsResponse({ entries: [JSON.parse(entry(1)), { bad: true }], warning: "logging store unavailable" });
    expect(r.entries).toHaveLength(1);
    expect(r.warning).toBe("logging store unavailable");
    expect(parseLogsResponse(null).entries).toEqual([]);
  });
});

describe("virtual window", () => {
  it("renders the visible rows plus overscan", () => {
    expect(visibleWindow(1000, 20, 2000, 200, 5)).toEqual({ start: 95, end: 116 });
    expect(visibleWindow(10, 20, 0, 200, 5)).toEqual({ start: 0, end: 10 });
    expect(visibleWindow(0, 20, 0, 200)).toEqual({ start: 0, end: 0 });
  });
});
