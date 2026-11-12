// @vitest-environment happy-dom
// The flight chrome's look as it renders: the flight-state chip is an outline
// in the state colour with a dot and the words in the panel's text colours,
// the status pill is a dot plus text with the severity's text colour, the
// alert rows sit on the dark panel, and every one of those text colours reads
// at 4.5:1 or better on the panel over a white picture.

import { createElement, type ReactElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";

import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { StatusPill } from "@/components/feed/cockpit/status-pill";
import { ALERT_TINT_PCT } from "@/lib/flight-layout";
import { chipLineTextColor } from "@/lib/chip-view";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import tokensJson from "@/shared/cockpit-spec/tokens.json";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { specText } from "@/shared/hud/spec-text";
import { severityTextColor } from "@/shared/hud/status-pill";
import { INITIAL_FLIGHT_STATE, useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import type { VehicleState } from "@/lib/types";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

async function render(el: ReactElement): Promise<HTMLElement> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(el));
  return host;
}

afterEach(async () => {
  if (host?.isConnected) {
    await act(async () => root.unmount());
    host.remove();
  }
  useNavStreamStore.getState().clear();
  useFlightStore.setState({ ...INITIAL_FLIGHT_STATE });
});

const PILOT = {
  owner: "idle",
  controlMode: "ados",
  profile: "normal",
  avoid: "brake",
  armed: false,
  landed: true,
  vehicleClass: "multirotor",
  readiness: [],
  planNed: [],
} as unknown as PilotState;

function pilot(p: Partial<PilotState> = {}) {
  useNavStreamStore.setState({ pilot: { data: { ...PILOT, ...p }, at: Date.now() } });
}

/** sRGB relative luminance and WCAG contrast. */
function rgb(hex: string): [number, number, number, number] {
  const h = hex.replace("#", "");
  const a = h.length === 8 ? parseInt(h.slice(6, 8), 16) / 255 : 1;
  return [parseInt(h.slice(0, 2), 16), parseInt(h.slice(2, 4), 16), parseInt(h.slice(4, 6), 16), a];
}
function over(top: string, under: [number, number, number]): [number, number, number] {
  const [r, g, b, a] = rgb(top);
  return [r * a + under[0] * (1 - a), g * a + under[1] * (1 - a), b * a + under[2] * (1 - a)];
}
function lum([r, g, b]: [number, number, number]): number {
  const c = (v: number) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * c(r) + 0.7152 * c(g) + 0.0722 * c(b);
}
function contrast(text: string, bg: [number, number, number]): number {
  const [a, b] = [lum(over(text, bg)), lum(bg)];
  return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
}

describe("text colours on the chrome panel over a white picture", () => {
  const panel = over(CHROME_TOKENS.panel_bg, [255, 255, 255]);

  it("every chip, pill and alert text colour reads at 4.5:1 or better", () => {
    const colours = new Set<string>([
      CHROME_TOKENS.text,
      CHROME_TOKENS.text_dim,
      severityTextColor("ok"),
      severityTextColor("warning"),
      severityTextColor("critical"),
      ...Object.values(tokensJson.state).map(chipLineTextColor),
    ]);
    for (const c of colours) expect(contrast(c, panel), c).toBeGreaterThanOrEqual(4.5);
  });

  it("the raw critical and pilot hues would not, so they stay on dots and outlines", () => {
    expect(contrast(CHROME_TOKENS.severity_critical, panel)).toBeLessThan(4.5);
    expect(contrast(tokensJson.state.pilot, panel)).toBeLessThan(4.5);
  });

  it("an alert row's tint stays at or under 18 %", () => {
    for (const pct of Object.values(ALERT_TINT_PCT)) expect(pct).toBeLessThanOrEqual(18);
  });
});

describe("the flight-state chip", () => {
  it("is an outline in the state colour with a dot, no fill, and the words in the text colour", async () => {
    pilot();
    const el = await render(createElement(FlightStateChip, { compact: false, fit: false }));
    const chip = el.querySelector<HTMLElement>('[data-testid="flight-state-chip"]')!;
    const dot = el.querySelector<HTMLElement>('[data-testid="flight-state-chip-dot"]')!;
    expect(dot).not.toBeNull();
    const hue = dot.style.background;
    expect(hue).not.toBe("");
    expect(chip.style.background).toBe("");
    expect(chip.className).toContain("bg-transparent");
    expect(chip.style.borderColor.toLowerCase()).toContain("99");
    expect(chip.className).toContain("h-[var(--fs-control)]");
    const words = chip.querySelector<HTMLElement>(".truncate")!;
    expect(words.style.color).toBe("var(--fs-text)");
  });

  it("keeps its width until the bar is full, and only then truncates", async () => {
    pilot();
    const roomy = await render(createElement(FlightStateChip, { compact: false, fit: false }));
    const box = roomy.querySelector<HTMLElement>('[data-testid="flight-state-chip-box"]')!;
    expect(box.className.split(" ")).toContain("shrink-0");
    await act(async () => root.unmount());
    host.remove();
    const full = await render(createElement(FlightStateChip, { compact: false, fit: true }));
    const box2 = full.querySelector<HTMLElement>('[data-testid="flight-state-chip-box"]')!;
    expect(box2.className.split(" ")).toContain("shrink");
    expect(box2.className.split(" ")).not.toContain("shrink-0");
  });
});

describe("the status pill", () => {
  const pillOf = (el: HTMLElement) => el.querySelector<HTMLElement>('[data-zone="status_pill"]')!;

  it("is a dot plus text, the text in the severity's text colour, with no fill", async () => {
    pilot();
    const el = await render(createElement(StatusPill, { compact: false, fit: false }));
    const pill = pillOf(el);
    expect(pill.querySelector('[data-testid="status-pill-dot"]')).not.toBeNull();
    expect(pill.querySelector("svg")).toBeNull();
    expect(pill.className).toContain("rounded-[var(--fs-radius-sm)]");
    expect(pill.className).not.toContain("rounded-full");
    const severity = pill.dataset.severity as "ok" | "warning" | "critical";
    expect(pill.style.color.toLowerCase()).toBe(severityTextColor(severity).toLowerCase());
  });

  it("hides the pre-flight items while the pilot's arm state is unknown", async () => {
    useFlightStore.setState({ telemetry: { armed: false } as VehicleState });
    pilot({ armed: false });
    const known = pillOf(await render(createElement(StatusPill, { compact: false, fit: false }))).getAttribute("aria-label") ?? "";
    expect(known).not.toContain(specText("cockpit.status.ok"));
    await act(async () => root.unmount());
    host.remove();
    pilot({ armed: null });
    const unknown = pillOf(await render(createElement(StatusPill, { compact: false, fit: false }))).getAttribute("aria-label") ?? "";
    expect(unknown).toContain(specText("cockpit.status.ok"));
  });

  it("raises a degraded navigation source", async () => {
    useFlightStore.setState({ telemetry: { armed: true, ekf: { flags: 0, age_ms: 100 } } as unknown as VehicleState, live: true });
    pilot({ armed: true, landed: false });
    const label = pillOf(await render(createElement(StatusPill, { compact: false, fit: false }))).getAttribute("aria-label") ?? "";
    expect(label).toContain(specText("alert.nav_source_degraded", { source: specText("nav_source.dr") }).slice(0, 20));
  });
});
