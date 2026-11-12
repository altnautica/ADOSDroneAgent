import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

// Text colours that fail 4.5:1 in one of the two themes.
const SRC = path.resolve(__dirname);
const files = (readdirSync(SRC, { recursive: true }) as string[])
  .map((f) => f.split(path.sep).join("/"))
  .filter((f) => /\.tsx?$/.test(f) && !f.startsWith("shared/") && !f.endsWith(".test.ts"));
const read = (f: string) => readFileSync(path.join(SRC, f), "utf8");

describe("dashboard text contrast in both themes", () => {
  it("never dims muted text below 4.5:1 with an alpha", () => {
    const hits = files.filter((f) => /text-muted-foreground\/(60|70)\b/.test(read(f)));
    expect(hits).toEqual([]);
  });

  it("uses no dark-only light-tint text literals", () => {
    const hits = files.filter((f) => /text-(amber|yellow|red|green|emerald|sky|blue)-(100|200|300)\b/.test(read(f)));
    expect(hits).toEqual([]);
  });

  it("draws the video empty state in an on-scrim colour", () => {
    const panel = read("components/panels/video-panel.tsx");
    const scrim = /className="[^"]*bg-scrim[^"]*"/.exec(panel)?.[0] ?? "";
    expect(scrim).toMatch(/text-white\/70/);
    expect(scrim).not.toMatch(/text-muted-foreground/);
  });

  it("shows the region chip only once the config is read, in warn tokens", () => {
    const home = read("routes/home.tsx");
    expect(home).toMatch(/if \(!config\.data \|\| config\.isError\) return null;/);
    expect(home).toMatch(/border-warn\/60 text-warn/);
  });
});
