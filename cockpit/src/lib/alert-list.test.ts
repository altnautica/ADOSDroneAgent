// The status pill's alert list follows the one layout every surface uses:
// heading "Alerts", each item's text first with its source label under it,
// and the spec's severity glyph (tokens `chrome.severity_icon`).

import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { CHROME_TOKENS, severityIcon } from "@/lib/chrome-tokens";
import tokens from "@/shared/cockpit-spec/tokens.json";
import { specText } from "@/shared/hud/spec-text";

const pill = readFileSync(path.resolve(__dirname, "../components/feed/cockpit/status-pill.tsx"), "utf8");

describe("the alert list", () => {
  it("draws the spec's severity glyphs", () => {
    expect(tokens.chrome.severity_icon).toEqual({ critical: "✕", warning: "⚠", info: "ℹ" });
    expect(severityIcon("critical")).toBe(CHROME_TOKENS.severity_icon.critical);
    expect(severityIcon("warning")).toBe("⚠");
    expect(severityIcon("ok")).toBe("ℹ");
    expect(pill).toContain("{severityIcon(a.severity)}");
  });

  it("is headed Alerts in the spec's casing", () => {
    expect(specText("cockpit.alerts.title")).toBe("Alerts");
    const heading = pill.split("\n").find((l) => l.includes("id={titleId}"));
    expect(heading).toBeDefined();
    expect(heading).not.toContain("uppercase");
  });

  it("puts each item's text first and its source label under it", () => {
    const text = pill.indexOf('data-testid="alert-item-text"');
    const source = pill.indexOf('data-testid="alert-item-source"');
    expect(text).toBeGreaterThan(0);
    expect(source).toBeGreaterThan(text);
  });
});
