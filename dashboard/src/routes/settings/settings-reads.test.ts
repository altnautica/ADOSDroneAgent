import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, "../..", f), "utf8");

describe("a failed settings read shows an error card, never a value", () => {
  it.each([
    ["routes/settings/display-settings.tsx", /opts\.isError \? \(\s*<ReadErrorCard/],
    ["routes/settings/sensors-settings.tsx", /sensors\.isError \? \(\s*<ReadErrorCard/],
    ["routes/settings/sensors-settings.tsx", /clock\.isError \? \(\s*<ReadErrorCard/],
    ["routes/settings/capture-settings.tsx", /storage\.isError \? \(\s*<ReadErrorCard/],
    ["routes/settings/mac-pin-settings.tsx", /config\.isError && \(\s*<ReadErrorCard/],
    ["components/wifi/wifi-panel.tsx", /scan\.isError && !scanning \? \(\s*<ReadErrorCard/],
  ])("%s", (file, branch) => {
    expect(read(file)).toMatch(branch);
  });

  it("marks the last known Wi-Fi and modem state stale after a failed read", () => {
    expect(read("components/wifi/wifi-panel.tsx")).toMatch(/statusStale && <Badge variant="warning">/);
    expect(read("routes/settings/cellular-settings.tsx")).toMatch(/loadFailed && modem && <Badge variant="warning">/);
  });
});

describe("cellular writes", () => {
  const src = read("routes/settings/cellular-settings.tsx");

  it("enable/disable and the APN wait for a confirm", () => {
    expect(src.match(/<ConfirmSwitch/g)).toHaveLength(2);
    expect(src).not.toMatch(/<Switch\b/);
    expect(src).toMatch(/confirm=\{\{ title: t\("settings\.cellular\.apnConfirmTitle"\)/);
    expect(src).toMatch(/onClick=\{\(\) => setAskApn\(true\)\}/);
  });

  it("re-seeds per value, so a refetch never wipes a typed APN", () => {
    expect(src).not.toMatch(/\}, \[config\.data\]\);/);
    expect(src).toMatch(/\}, \[read, initialApn\]\);/);
  });
});

describe("advanced settings", () => {
  it("re-seeds per value", () => {
    const src = read("routes/settings/advanced-settings.tsx");
    expect(src).not.toMatch(/\}, \[config\.data\]\);/);
    expect(src).toMatch(/\}, \[read, initialOverride\]\);/);
  });
});
