import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const SRC = readFileSync(path.resolve(__dirname, "cockpit-launcher.tsx"), "utf8");
const TOKENS = readFileSync(path.resolve(__dirname, "../../shared/styles/tokens.generated.css"), "utf8");

function lightToken(name: string): [number, number, number] {
  const block = TOKENS.slice(TOKENS.indexOf('html[data-theme="light"]'));
  const m = new RegExp(`--${name}:\\s*([\\d.]+)\\s+([\\d.]+)%\\s+([\\d.]+)%`).exec(block);
  if (!m) throw new Error(`no light --${name}`);
  const [h, s, l] = [Number(m[1]), Number(m[2]) / 100, Number(m[3]) / 100];
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => {
    const k = (n + h / 30) % 12;
    return 255 * (l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1)));
  };
  return [f(0), f(8), f(4)];
}

function luminance([r, g, b]: [number, number, number]): number {
  const c = (v: number) => {
    const x = v / 255;
    return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * c(r) + 0.7152 * c(g) + 0.0722 * c(b);
}

describe("Enter Cockpit card", () => {
  it("draws its icon accents with the theme's warn token, not dark-only amber literals", () => {
    expect(SRC).not.toMatch(/text-amber-\d+/);
    expect(SRC).toMatch(/text-warn/);
  });

  it("draws icon accents at 3:1 or better in the light theme", () => {
    const card = lightToken("card");
    const amber: [number, number, number] = [245, 158, 11];
    const tile = card.map((v, i) => v * 0.85 + amber[i] * 0.15) as [number, number, number];
    const fg = luminance(lightToken("warn"));
    const bg = luminance(tile);
    expect((Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05)).toBeGreaterThanOrEqual(3);
  });

  it("keeps the Launch label at 4.5:1 or better in the light theme", () => {
    const launch = /<span[^>]*>\s*\{t\("panels\.cockpit\.launch"\)\}/.exec(SRC)?.[0] ?? "";
    expect(launch).toMatch(/text-foreground/);
    // The pill: amber-500 at 10 % over the light card.
    const card = lightToken("card");
    const amber: [number, number, number] = [245, 158, 11];
    const pill = card.map((v, i) => v * 0.9 + amber[i] * 0.1) as [number, number, number];
    const fg = luminance(lightToken("foreground"));
    const bg = luminance(pill);
    expect((Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05)).toBeGreaterThanOrEqual(4.5);
  });
});
