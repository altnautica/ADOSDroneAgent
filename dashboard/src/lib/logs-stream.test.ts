import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it, vi } from "vitest";

import {
  EMPTY_TAIL,
  STREAM_RETRY_MS,
  logsReadFailed,
  scheduleStreamRetry,
  LOG_LEVELS,
  appendToTail,
  fmtLogTime,
  foldLevel,
  logsPollPath,
  parseSseChunk,
} from "./logs-stream";

const route = readFileSync(path.resolve(__dirname, "../routes/logs-route.tsx"), "utf8");

describe("log levels", () => {
  it("offers the levels the agent emits, no CRITICAL", () => {
    expect(LOG_LEVELS).toEqual(["", "DEBUG", "INFO", "WARN", "ERROR"]);
  });

  it("folds every producer's spelling into one bucket", () => {
    expect(foldLevel("warning")).toBe("WARN");
    expect(foldLevel("CRITICAL")).toBe("ERROR");
    expect(foldLevel("info")).toBe("INFO");
  });

  it("labels the filter through locale keys", () => {
    expect(route).toMatch(/t\(LEVEL_KEYS\[l\]\)/);
  });
});

describe("row text", () => {
  it("prints an RFC 3339 stamp as wall-clock time, never the raw string", () => {
    const text = fmtLogTime("2026-11-12T10:00:01.512345Z");
    expect(text).not.toContain("T10");
    expect(text).toMatch(/^\d{2}:\d{2}:\d{2}$/);
    expect(fmtLogTime(null)).toBe("—");
    expect(fmtLogTime("not a time")).toBe("not a time");
  });

  it("wraps the message instead of truncating it", () => {
    expect(route).toMatch(/whitespace-pre-wrap break-words/);
    expect(route).not.toMatch(/flex-1 truncate/);
  });
});

describe("live tail", () => {
  it("reads the stream with fetch and credentials, never EventSource", () => {
    expect(route).toMatch(/openLogStream\(level, service, ctrl\.signal/);
    expect(route).not.toMatch(/new EventSource/);
  });

  it("parses frames and drops replayed seqs", () => {
    const { events, rest } = parseSseChunk('data: {"seq":1,"level":"INFO","message":"a","logger":"x","timestamp":1}\n\ndata: par');
    expect(events).toHaveLength(1);
    expect(rest).toBe("data: par");
    const once = appendToTail(EMPTY_TAIL, events);
    expect(appendToTail(once, events)).toBe(once);
  });

  it("builds the poll path from the filters", () => {
    expect(logsPollPath("WARN", " ados ")).toBe("/api/logs?limit=200&level=WARN&service=ados");
  });
});

describe("log stream fallback", () => {
  it("reports a read failure when the stream is refused and the poll fails", () => {
    expect(logsReadFailed({ pollFallback: true, pollError: true })).toBe(true);
    // The poll answering, or the stream up, is not a failure.
    expect(logsReadFailed({ pollFallback: true, pollError: false })).toBe(false);
    expect(logsReadFailed({ pollFallback: false, pollError: true })).toBe(false);
  });

  it("tries the stream again on a timer while on the polling fallback", () => {
    vi.useFakeTimers();
    try {
      let retries = 0;
      const stop = scheduleStreamRetry(() => retries++);
      vi.advanceTimersByTime(STREAM_RETRY_MS - 1);
      expect(retries).toBe(0);
      vi.advanceTimersByTime(1);
      expect(retries).toBe(1);
      stop();
      vi.advanceTimersByTime(STREAM_RETRY_MS * 3);
      expect(retries).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });
});
