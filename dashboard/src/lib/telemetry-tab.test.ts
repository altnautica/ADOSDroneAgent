import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { telemetryTab } from "@/lib/telemetry-tab";

const ROUTE = readFileSync(path.resolve(__dirname, "../routes/telemetry-route.tsx"), "utf8");
const HOME = readFileSync(path.resolve(__dirname, "home-status.ts"), "utf8");
const SETUP = readFileSync(path.resolve(__dirname, "../routes/settings/aircraft-setup-settings.tsx"), "utf8");

describe("telemetry tabs", () => {
  it("reads the tab from ?tab=", () => {
    expect(telemetryTab("sensors")).toBe("sensors");
    expect(telemetryTab(null)).toBe("parameters");
    expect(telemetryTab("bogus")).toBe("parameters");
  });

  it("are a tablist with aria-selected tabs", () => {
    expect(ROUTE).toMatch(/role="tablist"/);
    expect(ROUTE.match(/role="tab"\n/g)?.length).toBe(2);
    expect(ROUTE).toMatch(/aria-selected=\{tab === "sensors"\}/);
  });
});

describe("small fixes", () => {
  it("never names the stick holder by the raw input_src code", () => {
    expect(HOME).not.toMatch(/inputSrc \?\? null/);
  });

  it("stacks aircraft setup below md", () => {
    expect(SETUP).toMatch(/layout=\{wide \? "split" : "stacked"\}/);
  });
});
