import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { MENU_ORDER, menuEntries, type MenuInputs } from "@/lib/quick-menu-model";
import layoutJson from "@/shared/cockpit-spec/layout.json";

const base: MenuInputs = { onFeed: true, movedCount: 0, cameraCount: 1, panelCount: 0, setupNeeded: false, setupOffered: false, kiosk: false };

describe("the Menu (layout.json menu.order)", () => {
  it("follows the spec's order", () => {
    expect(MENU_ORDER).toEqual(layoutJson.menu.order);
    expect(menuEntries({ ...base, movedCount: 2, cameraCount: 2, panelCount: 1, setupNeeded: true })).toEqual([
      "overflow",
      "view",
      "cameras",
      "layers",
      "preflight",
      "modes",
      "controls",
      "extension_panels",
      "setup",
      "settings",
      "exit",
    ]);
  });

  it("lists the top-bar items only when the bar has moved some", () => {
    expect(menuEntries(base)).not.toContain("overflow");
    expect(menuEntries({ ...base, movedCount: 1 })[0]).toBe("overflow");
  });

  it("offers Set up aircraft only while setup is needed, and no Exit on the kiosk", () => {
    expect(menuEntries(base)).not.toContain("setup");
    // The one-time setup offer stands in the Menu too, where panel and
    // controller input reach it (a banner button they cannot).
    expect(menuEntries({ ...base, setupOffered: true })).toContain("setup");
    expect(menuEntries({ ...base, kiosk: true })).not.toContain("exit");
    expect(menuEntries(base).at(-1)).toBe("exit");
  });

  it("offers Cameras with more than one leg and leaves the flight entries out off the Feed", () => {
    expect(menuEntries(base)).not.toContain("cameras");
    expect(menuEntries({ ...base, onFeed: false, movedCount: 2, cameraCount: 2 })).toEqual(["controls", "settings", "exit"]);
  });

  it("draws its words from the spec", () => {
    const src = readFileSync(path.resolve(__dirname, "../components/shell/quick-menu.tsx"), "utf8");
    for (const word of ['"Go to"', "Extension panels\n", "Exit to dashboard", "✕ select", "○ back", "D-pad move", "Options close"]) {
      expect(src, word).not.toContain(word);
    }
    // The Options comment follows ds4.json's binding (quick_settings: the Mode sheet).
    expect(src).toContain("`quick_settings`");
  });
});
