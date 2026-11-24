// @vitest-environment happy-dom
// The strip's `⋯` panel closes on Escape and on a press outside its anchor;
// a press inside (the toggle, the panel) leaves it open.

import { createElement, createRef, type RefObject } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import { usePanelDismiss } from "./use-panel-dismiss";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

function Panel({ open, close, anchor }: { open: boolean; close: () => void; anchor: RefObject<HTMLDivElement | null> }) {
  usePanelDismiss(open, anchor, close);
  return createElement("div", { ref: anchor, "data-testid": "anchor" }, createElement("button", { type: "button" }, "more"));
}

async function render(open: boolean, close: () => void) {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const anchor = createRef<HTMLDivElement>();
  await act(async () => root.render(createElement(Panel, { open, close, anchor })));
}

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  document.body.innerHTML = "";
});

const pointerDown = (el: Element) => el.dispatchEvent(new Event("pointerdown", { bubbles: true }));

describe("usePanelDismiss", () => {
  it("closes on Escape, and keeps the shell from also seeing it", async () => {
    const close = vi.fn();
    await render(true, close);
    const shell = vi.fn();
    document.addEventListener("keydown", shell);
    document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    document.removeEventListener("keydown", shell);
    expect(close).toHaveBeenCalledTimes(1);
    expect(shell).not.toHaveBeenCalled();
  });

  it("closes on a press outside the anchor, not on one inside", async () => {
    const close = vi.fn();
    await render(true, close);
    pointerDown(host.querySelector("button")!);
    expect(close).not.toHaveBeenCalled();
    const outside = document.createElement("div");
    document.body.appendChild(outside);
    pointerDown(outside);
    expect(close).toHaveBeenCalledTimes(1);
  });

  it("listens only while open", async () => {
    const close = vi.fn();
    await render(false, close);
    document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    pointerDown(document.body);
    expect(close).not.toHaveBeenCalled();
  });
});
