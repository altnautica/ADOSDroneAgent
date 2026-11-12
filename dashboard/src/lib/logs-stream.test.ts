import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import {
  EMPTY_TAIL,
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
