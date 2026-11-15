import { readFileSync } from "node:fs";
import path from "node:path";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { PlaceCard } from "@/components/panels/place-card";
import { choosePlace, placeCard } from "@/lib/place-card";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import { chipFacts, flightStateChip } from "@/shared/hud/flight-state";

// The golden frames the agent's own tests use, read in place.
const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const fixture = (name: string): unknown => JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8"));
const confined = parsePilotState(fixture("pilot-frame-confined.json"))!;
const open = parsePilotState(fixture("pilot-frame.json"))!;
const margins = { obstacleMarginM: 0.3, propTipRadiusM: 0.377 };

const render = (pilot = confined): string => {
  const card = placeCard(pilot, margins);
  return card ? renderToStaticMarkup(createElement(PlaceCard, { card, send: () => Promise.resolve({ ok: true }) })) : "";
};

describe("the Home place card on the golden confined frame", () => {
  it("shows the Confined badge, the class picked and its explain line", () => {
    const card = placeCard(confined, margins)!;
    expect(card.badge).toBe("Confined");
    expect(card.options.map((o) => [o.label, o.action, o.selected])).toEqual([
      ["Open", "place_class:open", false],
      ["Confined space", "place_class:confined", true],
    ]);
    expect(card.explain).toBe("Mines, tunnels and rooms: 0.48 m from walls, 0.5 m/s near the limit. Choose it only for a confined place.");
    const html = render();
    expect(html).toContain('data-testid="confined-chip"');
    expect(html).toMatch(/data-action="place_class:confined"[^>]*>Confined space<\/button>/);
  });

  it("reports the too-tight route with its pinch", () => {
    const html = render();
    expect(html).toContain('data-state="infeasible"');
    expect(html).toContain("Route too tight: leg 3→4 has 0.52 m, needs 0.58 m");
    expect(html).toContain("At N 41.3 m, E -17.5 m, height 2.0 m");
    expect(html).toMatch(/role="alert"[^>]*data-testid="route-check"/);
  });

  it("reads Open with no badge and no report in an open place", () => {
    const card = placeCard(open, margins)!;
    expect(card.badge).toBeNull();
    expect(card.routeCheck).toBeNull();
    expect(card.explain).toBe("Open sites: 0.5 m from obstacles.");
    expect(render(open)).not.toContain('data-testid="route-check"');
  });

  it("is absent without a fresh pilot", () => {
    expect(placeCard(null, margins)).toBeNull();
  });
});

describe("picking a place class", () => {
  it("sends place_class:<class> for a class not in force", async () => {
    const sent: string[] = [];
    const send = (a: string) => {
      sent.push(a);
      return Promise.resolve({ ok: true });
    };
    const [openOpt, confinedOpt] = placeCard(open, margins)!.options;
    expect(await choosePlace(confinedOpt, send)).toBeNull();
    expect(await choosePlace(openOpt, send)).toBeNull();
    expect(sent).toEqual(["place_class:confined"]);
  });

  it("says why the pilot refused it", async () => {
    const [, confinedOpt] = placeCard(open, margins)!.options;
    expect(await choosePlace(confinedOpt, () => Promise.resolve({ ok: false, err: "no_prop_radius" }))).toBe(
      "Confined space: Set the vehicle's outer radius (props, guards or cage) for the confined profile",
    );
  });
});

describe("the Home flight chip in a confined place", () => {
  it("carries the confined-near cap", () => {
    const chip = flightStateChip(chipFacts(confined, 0, { homeDistM: null, modeDetail: null, completeAgeMs: null }));
    expect(chip.secondary.map((l) => l.text)).toContain("Near the clearance limit: 0.5 m/s max");
  });
});
