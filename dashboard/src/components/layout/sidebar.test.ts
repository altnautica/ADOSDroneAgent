// The desktop sidebar's collapse button: it flips the UI store's persisted
// `sidebarCollapsed` (labelled w-56 ↔ icons only w-16) through `toggleSidebar`.

import { isValidElement, type ReactElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

import { useUiStore } from "@/stores/ui-store";

import { SidebarToggle } from "./sidebar";
import { sidebarAsideClass } from "./sidebar-layout";

describe("the sidebar collapse button", () => {
  it("calls toggleSidebar, which flips the collapsed state each press", () => {
    const toggle = vi.spyOn(useUiStore.getState(), "toggleSidebar");
    useUiStore.setState({ sidebarCollapsed: false });
    const el = SidebarToggle({ collapsed: false, label: "Collapse sidebar" });
    expect(isValidElement(el)).toBe(true);
    const press = (el as ReactElement<{ onClick: () => void }>).props.onClick;
    press();
    expect(toggle).toHaveBeenCalledTimes(1);
    expect(useUiStore.getState().sidebarCollapsed).toBe(true);
    press();
    expect(useUiStore.getState().sidebarCollapsed).toBe(false);
  });

  it("names what it does and reports the expanded state", () => {
    const open = renderToStaticMarkup(SidebarToggle({ collapsed: false, label: "Collapse sidebar" }));
    expect(open).toContain('aria-label="Collapse sidebar"');
    expect(open).toContain('aria-expanded="true"');
    const shut = renderToStaticMarkup(SidebarToggle({ collapsed: true, label: "Expand sidebar" }));
    expect(shut).toContain('aria-expanded="false"');
  });
});

describe("the desktop aside", () => {
  it("sticks under the header at the viewport height, so the collapse button stays in view", () => {
    for (const collapsed of [false, true]) {
      const cls = sidebarAsideClass(collapsed).split(" ");
      expect(cls).toEqual(expect.arrayContaining(["sticky", "top-[57px]", "h-[calc(100dvh-57px)]", "self-start", "overflow-hidden"]));
      expect(cls).toContain(collapsed ? "w-16" : "w-56");
    }
  });
});
