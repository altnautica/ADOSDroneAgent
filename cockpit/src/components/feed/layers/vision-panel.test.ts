import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { effectiveLayers, forcingView } from "@/lib/cockpit-view";

import { LayerSwitch } from "./vision-panel";

/** The `<input>` tags of a rendered layer switch. */
function inputs(html: string): string[] {
  return html.match(/<input[^>]*>/g) ?? [];
}

function render(forcedBy: string | null, stored: boolean): string {
  return renderToStaticMarkup(createElement(LayerSwitch, { id: "depth", label: "Depth", forcedBy, stored, onToggle: () => {} }));
}

describe("the vision panel's layer switches", () => {
  it("name the Vision view as forcing depth and surfaces on, as views.json says, and no other view", () => {
    expect(forcingView("vision", "depth")).toBe("Vision");
    expect(forcingView("vision", "surfaces")).toBe("Vision");
    for (const view of ["pilot", "map", "split"] as const) expect(forcingView(view, "depth")).toBeNull();
    // What the switch shows is what is drawn: the Vision view draws depth
    // with its flag off.
    expect(effectiveLayers("full", { depth: false }, "vision").has("depth")).toBe(true);
  });

  it("read on and disabled, with the reason, while the view forces the layer on", () => {
    // The flag is off, as on the bench's cockpit, where the switch read off.
    const html = render("Vision", false);
    const [forced, keep] = inputs(html);
    expect(forced).toContain('checked=""');
    expect(forced).toContain('disabled=""');
    expect(forced).toContain('aria-describedby="layer-depth-forced"');
    expect(html).toContain('id="layer-depth-forced"');
    expect(html).toContain("On while the Vision view is open");
    // The flag for the other views stays the operator's, and settable.
    expect(keep).not.toContain("checked");
    expect(keep).not.toContain("disabled");
    expect(inputs(render("Vision", true))[1]).toContain('checked=""');
  });

  it("follow the stored flag where nothing forces the layer", () => {
    expect(inputs(render(null, true))).toEqual(['<input type="checkbox" checked=""/>']);
    expect(inputs(render(null, false))).toEqual(['<input type="checkbox"/>']);
    expect(render(null, false)).not.toContain("view is open");
  });
});
