// @vitest-environment happy-dom
// The Feed's width class comes from its own box: framed beside the menu rail
// it is narrower than the window, full screen it is the whole window.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import { CockpitShell } from "@/components/shell/cockpit-shell";
import { useWidthClass } from "@/hooks/use-width-class";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";

vi.mock("@/lib/kiosk", () => ({ isKiosk: () => false }));
vi.mock("@/hooks/use-status-poll", () => ({ useStatusPoll: () => undefined }));
vi.mock("@/components/shell/menu-rail", () => ({ MenuRail: () => createElement("nav", { "data-testid": "menu-rail" }) }));
vi.mock("@/shared/use-profile", () => ({ useNodeIdentity: () => ({ profile: "drone", role: null }) }));
vi.mock("@/nav/registry", () => {
  const Body = () => createElement("div", { "data-testid": "feed-class" }, useWidthClass().widthClass);
  const feed = { id: "feed", title: "Feed", kind: "tab", fullBleed: true, render: () => createElement(Body) };
  return { getScreen: () => feed, homeTabFor: () => "feed", tabScreens: () => [feed] };
});

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

/** The window is 900 px wide (expanded); the menu rail takes 106 px of it. */
const WINDOW_W = 900;
const RAIL_W = 106;
const realRect = HTMLElement.prototype.getBoundingClientRect;

let host: HTMLDivElement | null = null;
let root: Root;

afterEach(async () => {
  if (host) {
    await act(async () => root.unmount());
    host.remove();
    host = null;
  }
  HTMLElement.prototype.getBoundingClientRect = realRect;
  vi.unstubAllGlobals();
});

describe("the Feed's width class", () => {
  it("reads its own box beside the menu rail, and the whole window full screen", async () => {
    Object.assign(window, { innerWidth: WINDOW_W, innerHeight: 600 });
    HTMLElement.prototype.getBoundingClientRect = function () {
      const framed = this.parentElement?.dataset.shell === "flight-framed";
      const w = this.tagName === "MAIN" ? (framed ? WINDOW_W - RAIL_W : WINDOW_W) : 0;
      return { x: 0, y: 0, left: 0, top: 0, width: w, height: 600, right: w, bottom: 600, toJSON: () => ({}) } as DOMRect;
    };
    // happy-dom lays nothing out, so its ResizeObserver never fires: this one
    // reports each observed element's box when `resized()` is called.
    const observed: { el: Element; cb: ResizeObserverCallback }[] = [];
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(private cb: ResizeObserverCallback) {}
        observe(el: Element) {
          observed.push({ el, cb: this.cb });
        }
        disconnect() {
          observed.length = 0;
        }
      },
    );
    const resized = () => {
      for (const { el, cb } of observed) {
        const r = el.getBoundingClientRect();
        cb([{ target: el, contentRect: r } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    };
    useCockpitUiStore.setState({ flightFullscreen: false });
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    await act(async () => root.render(createElement(CockpitShell, { input: { buttonsConnected: false, gamepadConnected: false, wakeHeld: false } })));
    const cls = () => host!.querySelector('[data-testid="feed-class"]')?.textContent;
    expect(cls()).toBe("medium");
    await act(async () => useCockpitUiStore.setState({ flightFullscreen: true }));
    await act(async () => resized());
    expect(cls()).toBe("expanded");
  });
});
