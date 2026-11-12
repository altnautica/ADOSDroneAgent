// @vitest-environment happy-dom
// Where the shell puts the Feed: framed beside the menu rail in a browser,
// full bleed on the kiosk, and full bleed after the top bar's Full screen
// button (back to framed when the browser leaves full screen).

import { createElement, type ComponentType } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const kiosk = vi.hoisted(() => ({ on: false }));
const screen = vi.hoisted(() => ({ body: null as null | ComponentType }));

// The first case loads the whole shell's module graph cold.
vi.setConfig({ testTimeout: 30_000 });

vi.mock("@/lib/kiosk", () => ({ isKiosk: () => kiosk.on }));
vi.mock("@/hooks/use-status-poll", () => ({ useStatusPoll: () => undefined }));
vi.mock("@/components/shell/menu-rail", () => ({ MenuRail: () => createElement("nav", { "data-testid": "menu-rail" }) }));
vi.mock("@/shared/use-profile", () => ({ useNodeIdentity: () => ({ profile: "drone", role: null }) }));
vi.mock("@/nav/registry", () => {
  const feed = { id: "feed", title: "Feed", kind: "tab", fullBleed: true, render: () => createElement(screen.body!) };
  return { getScreen: () => feed, homeTabFor: () => "feed", tabScreens: () => [feed] };
});

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement | null = null;
let root: Root;

async function mount(): Promise<HTMLElement> {
  // Imported per case after `vi.resetModules()`: the kiosk flag is read when
  // the UI store module loads, so each case needs a fresh module graph, and
  // the screen's top bar must come from that same graph.
  screen.body = (await import("@/components/feed/cockpit/top-bar")).TopBar;
  const { CockpitShell } = await import("@/components/shell/cockpit-shell");
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(CockpitShell, { input: { buttonsConnected: false, gamepadConnected: false, wakeHeld: false } })));
  return host;
}

const shellOf = (el: HTMLElement) => el.querySelector<HTMLElement>("[data-shell]")?.dataset.shell;

beforeEach(() => {
  vi.resetModules();
});

afterEach(async () => {
  if (host) {
    await act(async () => root.unmount());
    host.remove();
    host = null;
  }
});

describe("the shell around the Feed", () => {
  it("frames the Feed beside the menu rail in a browser", async () => {
    kiosk.on = false;
    const el = await mount();
    expect(shellOf(el)).toBe("flight-framed");
    expect(el.querySelector('[data-testid="menu-rail"]')).not.toBeNull();
    expect(el.querySelector('[data-testid="top-bar-fullscreen"]')).not.toBeNull();
  });

  it("draws the Feed full bleed on the kiosk, with no Full screen button", async () => {
    kiosk.on = true;
    const el = await mount();
    expect(shellOf(el)).toBe("flight");
    expect(el.querySelector('[data-testid="menu-rail"]')).toBeNull();
    expect(el.querySelector('[data-testid="top-bar-fullscreen"]')).toBeNull();
  });

  it("goes full bleed on the Full screen button and back when the browser leaves full screen", async () => {
    kiosk.on = false;
    const { watchFullscreenExit } = await import("@/lib/fullscreen");
    const stop = watchFullscreenExit();
    const el = await mount();
    await act(async () => el.querySelector<HTMLButtonElement>('[data-testid="top-bar-fullscreen"]')!.click());
    expect(shellOf(el)).toBe("flight");
    expect(el.querySelector('[data-testid="menu-rail"]')).toBeNull();
    await act(async () => {
      document.dispatchEvent(new Event("fullscreenchange"));
    });
    expect(shellOf(el)).toBe("flight-framed");
    stop();
  });
});
