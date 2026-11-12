import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { chipTextColor } from "@/lib/chip-color";

const TILES = readFileSync(path.resolve(__dirname, "../components/panels/status-tiles.tsx"), "utf8");

describe("flight chip on the dashboard card", () => {
  it("mixes the state hue toward the theme foreground for text", () => {
    expect(chipTextColor("#2979FF")).toBe("color-mix(in oklab, #2979FF 62%, hsl(var(--foreground)))");
  });

  it("never paints chip text in the raw overlay hue, and stays one line", () => {
    expect(TILES).not.toMatch(/style=\{\{ color: (chip|l)\.color/);
    expect(TILES).toMatch(/max-w-full truncate rounded-full border/);
    expect(TILES).toMatch(/title=\{chip\.text\}/);
  });
});
