import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const src = readFileSync(path.resolve(__dirname, "status-tiles.tsx"), "utf8");

describe("Home vehicle status: the follow line", () => {
  it("reads the fresh follow frame through the shared rule", () => {
    expect(src).toMatch(/homeFollow\(fresh\.follow\)/);
  });

  it("renders the phase word, the waiting prompt and its body", () => {
    expect(src).toMatch(/\{follow\.word\}/);
    expect(src).toMatch(/\{follow\.prompt\}/);
    expect(src).toMatch(/\{follow\.body\}/);
  });

  it("offers the prompt's resume and stop through the plugin config, with the spec's words", () => {
    expect(src).toMatch(/follow\.actions\.map\(/);
    expect(src).toMatch(/sendFollowAction\(a, follow\.resumeSeq\)/);
    expect(src).toMatch(/specText\(`follow\.prompt\.action\.\$\{a\}`\)/);
  });
});
