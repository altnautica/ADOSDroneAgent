import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import "@/shared/i18n";
import { effectiveLayers, forcingView } from "@/lib/cockpit-view";
import type { VisionEngineStatus } from "@/stores/layer-chips-store";

import { LayerSwitch } from "./vision-panel";
import { AGENT_RESTART_MS, detectorCandidates, engineCardState, layerLiveState, runningDetector, selectedDetector, VISION_START_MS } from "./vision-state";

/** The `<input>` tags of a rendered layer switch. */
function inputs(html: string): string[] {
  return html.match(/<input[^>]*>/g) ?? [];
}

function render(forcedBy: string | null, stored: boolean, live?: "receiving" | "no_data" | "engine_off"): string {
  return renderToStaticMarkup(createElement(LayerSwitch, { id: "depth", label: "Depth", forcedBy, stored, live, onToggle: () => {} }));
}

describe("the vision panel's layer rows", () => {
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
    expect(html).toContain("Keep on in other views");
    // The flag for the other views stays the operator's, and settable.
    expect(keep).not.toContain("checked");
    expect(keep).not.toContain("disabled");
    expect(inputs(render("Vision", true))[1]).toContain('checked=""');
  });

  it("follow the stored flag where nothing forces the layer", () => {
    const on = inputs(render(null, true));
    expect(on).toHaveLength(1);
    expect(on[0]).toContain('checked=""');
    expect(on[0]).not.toContain("disabled");
    const off = inputs(render(null, false));
    expect(off).toHaveLength(1);
    expect(off[0]).not.toContain("checked");
    expect(render(null, false)).not.toContain("view is open");
  });

  it("show the live status only while the layer is on", () => {
    expect(render(null, true, "receiving")).toContain("Receiving");
    expect(render(null, true, "engine_off")).toContain("Engine off");
    expect(render(null, false, "no_data")).not.toContain("No data");
  });

  it("read receiving, no data or engine off from the frame clock and the engine", () => {
    expect(layerLiveState(1000, true, 2500, true)).toBe("receiving");
    expect(layerLiveState(1000, true, 3000, true)).toBe("no_data");
    expect(layerLiveState(0, false, 10, null)).toBe("no_data");
    expect(layerLiveState(1000, true, 1500, false)).toBe("engine_off");
  });
});

const UP: VisionEngineStatus = { backend: "onnx", backend_state: "ready", backend_reason: null, models: [] };

describe("the vision engine card", () => {
  const base = { enabled: true, status: null, error: false, pending: null, now: 100_000 } as const;

  it("reads running whenever the engine answers with a usable backend", () => {
    expect(engineCardState({ ...base, status: UP })).toBe("running");
    expect(engineCardState({ ...base, status: UP, pending: { kind: "starting", since: 99_000 } })).toBe("running");
  });

  it("offers Turn on when vision is off, and Restart when it is on but silent", () => {
    expect(engineCardState({ ...base, enabled: false, error: true })).toBe("off");
    expect(engineCardState({ ...base, error: true })).toBe("unreachable");
    expect(engineCardState({ ...base, status: { ...UP, backend_state: "unavailable" } })).toBe("unavailable");
    expect(engineCardState(base)).toBe("checking");
  });

  it("shows starting for up to 30 s after a start, then says it did not start", () => {
    const since = base.now;
    expect(engineCardState({ ...base, error: true, pending: { kind: "starting", since } })).toBe("starting");
    expect(engineCardState({ ...base, error: true, pending: { kind: "starting", since }, now: since + VISION_START_MS })).toBe("start_timeout");
  });

  it("asks for an agent restart when the agent refused to restart vision", () => {
    expect(engineCardState({ ...base, error: true, pending: { kind: "needs_agent_restart" } })).toBe("needs_agent_restart");
    const since = base.now;
    expect(engineCardState({ ...base, error: true, pending: { kind: "agent_restarting", since } })).toBe("agent_restarting");
    expect(engineCardState({ ...base, error: true, pending: { kind: "agent_restarting", since }, now: since + AGENT_RESTART_MS })).toBe(
      "start_timeout",
    );
  });
});

describe("the detector model picker", () => {
  // The bench's list: the registry was never fetched, so no file carries a task.
  const bare = [
    { id: "midas_v21_small_256" },
    { id: "osnet_x0_25_reid_256x128" },
    { id: "yolox_tiny_person_320" },
    { id: "yolox_tiny_person_640" },
  ];

  it("lists detection models only: by task, else not by a depth or re-id model's name", () => {
    expect(detectorCandidates(bare).map((m) => m.id)).toEqual(["yolox_tiny_person_320", "yolox_tiny_person_640"]);
    expect(detectorCandidates([{ id: "a", task: "depth" }, { id: "b", task: "Detection" }, { id: "c", task: "reid" }]).map((m) => m.id)).toEqual(["b"]);
  });

  it("marks the chosen model, else the engine's running detector matched by id or registry id", () => {
    const engine: VisionEngineStatus = {
      backend: "rknn",
      backend_state: "ready",
      backend_reason: null,
      models: [
        { id: "reid_osnet_x0_25_v1", kind: "tracking", fps: null, latency_ms: null, backend_loaded: true },
        { id: "person_yolox_tiny_v1", kind: "detection", fps: null, latency_ms: null, backend_loaded: true },
      ],
    };
    expect(runningDetector(engine)).toBe("person_yolox_tiny_v1");
    expect(selectedDetector({ installed: bare, active: "yolox_tiny_person_640" }, runningDetector(engine))).toBe("yolox_tiny_person_640");
    const registered = [{ id: "yolox_tiny_person_320", task: "detection", registry_id: "person_yolox_tiny_v1" }];
    expect(selectedDetector({ installed: registered, active: null }, runningDetector(engine))).toBe("yolox_tiny_person_320");
    // A depth model is never the detector, whatever is configured.
    expect(selectedDetector({ installed: bare, active: "midas_v21_small_256" }, null)).toBeNull();
  });
});
