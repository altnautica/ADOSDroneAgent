import { describe, expect, it } from "vitest";

import { LOG_LEVELS, fmtLogTime, foldLevel } from "@/lib/logs-stream";

describe("log levels and times", () => {
  it("offers only the levels the agent emits", () => {
    expect(LOG_LEVELS).toEqual(["", "DEBUG", "INFO", "WARN", "ERROR"]);
  });

  it("folds other spellings into the filter's buckets", () => {
    expect(foldLevel("WARNING")).toBe("WARN");
    expect(foldLevel("critical")).toBe("ERROR");
    expect(foldLevel("INFO")).toBe("INFO");
  });

  it("shows an RFC 3339 stamp as a local time, not the raw string", () => {
    const out = fmtLogTime("2026-11-12T12:15:47.900Z");
    expect(out).not.toContain("T");
    expect(out).toMatch(/^\d{2}:\d{2}:\d{2}$/);
    expect(fmtLogTime("not a time")).toBe("not a time");
  });
});
