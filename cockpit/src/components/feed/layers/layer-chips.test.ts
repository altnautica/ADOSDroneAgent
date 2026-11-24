import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it } from "vitest";

import "@/shared/i18n";
import { CHIP_IDS, layerChips, type ChipId } from "@/shared/hud/layer-engine";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";

import { badgeTurnOff, turnOffBadge } from "./layer-badge-actions";
import { BadgeMenuBody } from "./layer-chips";

const chip = (id: ChipId, engineUp: boolean | null = true) => layerChips(1 << CHIP_IDS.indexOf(id), engineUp)[0];

function menu(id: ChipId, view: "pilot" | "vision", opts: { engineUp?: boolean; canSetUp?: boolean } = {}): string {
  return renderToStaticMarkup(
    createElement(BadgeMenuBody, { chip: chip(id, opts.engineUp ?? true), view, canSetUp: opts.canSetUp ?? true, onClose: () => {} }),
  );
}

beforeEach(() => {
  useCockpitViewStore.setState({ view: "pilot", layers: {} });
});

describe("a layer badge's menu", () => {
  it("offers Turn off where nothing forces the layer, and Close Vision view where the open view does", () => {
    expect(badgeTurnOff(chip("depth"), "pilot")).toBe("turn_off");
    expect(badgeTurnOff(chip("depth"), "vision")).toBe("close_view");
    expect(badgeTurnOff(chip("targets"), "vision")).toBe("close_view");
    // The Vision view forces depth, surfaces and targets, never obstacles or AR.
    expect(badgeTurnOff(chip("obstacles"), "vision")).toBe("turn_off");
    expect(badgeTurnOff(chip("ar"), "vision")).toBe("turn_off");

    const pilot = menu("depth", "pilot");
    expect(pilot).toContain("Turn off");
    expect(pilot).not.toContain("Close Vision view");
    const vision = menu("depth", "vision");
    expect(vision).toContain("Close Vision view");
    expect(vision).not.toContain("Turn off");
  });

  it("says why the badge shows", () => {
    expect(menu("depth", "pilot", { engineUp: false })).toContain("Vision engine is off");
    expect(menu("depth", "pilot")).toContain("No data from Depth for 2 s");
    expect(menu("obstacles", "pilot", { engineUp: false })).toContain("No data from Obstacles for 2 s");
  });

  it("offers Set up only where Settings › Vision exists", () => {
    expect(menu("depth", "pilot")).toContain("Set up");
    expect(menu("depth", "pilot", { canSetUp: false })).not.toContain("Set up");
  });

  it("Turn off switches every flag the chip stands for off", () => {
    turnOffBadge(chip("ar"), "pilot");
    expect(useCockpitViewStore.getState().layers).toEqual({ ar_route: false, ar_motion: false });
    expect(useCockpitViewStore.getState().view).toBe("pilot");
  });

  it("Close Vision view goes back to the Pilot view and leaves the flags alone", () => {
    useCockpitViewStore.setState({ view: "vision" });
    turnOffBadge(chip("depth"), "vision");
    expect(useCockpitViewStore.getState().view).toBe("pilot");
    expect(useCockpitViewStore.getState().layers).toEqual({});
  });
});
