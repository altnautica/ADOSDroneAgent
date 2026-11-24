// @vitest-environment happy-dom
// The hidden rail's edge tab must read on dark video: a filled tab with a
// chevron, inside a 48 px wide hit area.

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import "@/shared/i18n";

import { RailEdgeTab } from "./menu-rail";

describe("the hidden rail's edge tab", () => {
  const html = renderToStaticMarkup(createElement(RailEdgeTab, { onShow: () => {} }));
  const doc = new DOMParser().parseFromString(html, "text/html");
  const button = doc.querySelector('[data-testid="rail-edge-tab"]');
  const fill = doc.querySelector('[data-testid="rail-edge-tab-fill"]');

  it("keeps a 48 px wide tap area", () => {
    expect(button?.className.split(" ")).toEqual(expect.arrayContaining(["w-[48px]", "h-[64px]"]));
    expect(button?.getAttribute("aria-label")).toBeTruthy();
  });

  it("is filled in a contrasting colour and carries a chevron", () => {
    expect(fill?.className.split(" ")).toEqual(expect.arrayContaining(["bg-primary", "text-primary-foreground"]));
    expect(fill?.querySelector("svg")).not.toBeNull();
  });
});
