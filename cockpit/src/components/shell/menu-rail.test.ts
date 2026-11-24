// @vitest-environment happy-dom
// The labelled rail: a short label under each icon, the full page name as the
// entry's accessible name, one Hide control, and a width in rem so it keeps
// its proportion to the bounded root (lib/ui-scale.ts) at every size.

import { readFileSync } from "node:fs";
import path from "node:path";
import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot } from "react-dom/client";
import { afterAll, describe, expect, it, vi } from "vitest";

import i18n from "@/shared/i18n";
import en from "@/shared/locales/en.json";
import { ROOT_FONT } from "@/lib/ui-scale";
import { SCREEN_TABLE } from "@/nav/screen-table";
import { useSettingsStore } from "@/stores/settings-store";

import { MenuRail } from "./menu-rail";

const TABS = SCREEN_TABLE.filter((s) => s.kind === "tab");

vi.mock("@/shared/use-profile", () => ({ useNodeIdentity: () => ({ profile: "drone", role: null }) }));
vi.mock("@/nav/registry", () => ({
  tabScreens: () => TABS.map((s) => ({ ...s, title: i18n.t(s.labelKey) })),
}));

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const host = document.createElement("div");
document.body.appendChild(host);
const root = createRoot(host);

afterAll(async () => {
  await act(async () => root.unmount());
  host.remove();
});

describe("the labelled menu rail", () => {
  it("has a short rail label for every tab", () => {
    for (const tab of TABS) expect((en.rail.label as Record<string, string>)[tab.id], tab.id).toBeTruthy();
  });

  it("shows the short label and keeps the full page name as the accessible name", async () => {
    useSettingsStore.getState().setRail("labelled");
    await act(async () => root.render(createElement(MenuRail)));
    const items = [...host.querySelectorAll<HTMLButtonElement>('[data-testid="rail-item"]')];
    const transmit = items.find((b) => b.getAttribute("aria-label") === en.nav.transmit);
    expect(transmit?.querySelector('[data-testid="rail-label"]')?.textContent).toBe(en.rail.label.transmit);
    expect(transmit?.title).toBe(en.nav.transmit);
    const params = items.find((b) => b.getAttribute("aria-label") === en.nav.params);
    expect(params?.textContent).toBe(en.rail.label.params);
    for (const b of items) expect(b.className).toContain("min-h-[calc(48px*var(--touch-scale))]");
  });

  it("hides behind the edge tab from its Hide control, and the tab brings it back", async () => {
    useSettingsStore.getState().setRail("labelled");
    await act(async () => root.render(createElement(MenuRail)));
    const hide = host.querySelector<HTMLButtonElement>('[data-testid="rail-toggle"]')!;
    expect(hide.getAttribute("aria-label")).toBe(en.rail.hide);
    await act(async () => hide.click());
    expect(useSettingsStore.getState().rail).toBe("hidden");
    expect(host.querySelector('[data-testid="menu-rail"]')).toBeNull();
    await act(async () => host.querySelector<HTMLButtonElement>('[data-testid="rail-edge-tab"]')!.click());
    expect(useSettingsStore.getState().rail).toBe("labelled");
  });

  it("is sized in rem against the bounded root: 72-99 px wide", () => {
    const css = readFileSync(path.resolve(__dirname, "../../styles/globals.css"), "utf8");
    const m = /--chrome-rail:\s*calc\(([\d.]+)rem \* var\(--touch-scale\)\);/.exec(css);
    expect(m).not.toBeNull();
    const rem = Number(m![1]);
    expect(rem * ROOT_FONT.minPx).toBeGreaterThanOrEqual(64);
    expect(rem * ROOT_FONT.maxPx).toBeLessThanOrEqual(100);
  });
});
