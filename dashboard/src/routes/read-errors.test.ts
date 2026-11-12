import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("a failed read never shows the empty state", () => {
  it.each([
    ["routes/extensions-route.tsx", /list\.isError && \(\s*<ReadErrorCard/, /!list\.isError && installs\.length === 0/],
    ["routes/peripherals-route.tsx", /list\.isError && \(\s*<ReadErrorCard/, /!list\.isError && items\.length === 0/],
    ["routes/video-route.tsx", /cameras\.isError \? \(\s*<ReadErrorCard/, /results\.some\(\(r\) => r\.isError\)/],
    ["routes/io-route.tsx", /bluetooth\.isError \? \(\s*<ReadErrorCard/, /io\.bluetooth\.readFailed/],
  ])("%s", (file, errorBranch, guard) => {
    const src = read(file);
    expect(src).toMatch(errorBranch);
    expect(src).toMatch(guard);
  });

  it("peripherals 501 reads as the absent service", () => {
    expect(read("routes/peripherals-route.tsx")).toMatch(/status === 501\s*\? t\("cockpit\.peripherals\.residualAbsent"\)/);
  });
});
