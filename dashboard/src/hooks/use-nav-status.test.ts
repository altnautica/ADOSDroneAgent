import { describe, expect, it } from "vitest";

import { defaultLeasedLayers, layersViewQuery, LEASED_LAYERS } from "@/lib/video-layer-lease";
import { NAV_POLL_IDLE_MS, NAV_POLL_VIDEO_MS, navPollMs, navStatusPath } from "@/hooks/use-nav-status";
import { skillDetailFromState, skillStateTopic } from "@/lib/skill-detail";

describe("shared nav-status poll", () => {
  it("polls at 5 Hz while a video panel is mounted, else 1 Hz", () => {
    expect(navPollMs(1)).toBe(NAV_POLL_VIDEO_MS);
    expect(NAV_POLL_VIDEO_MS).toBe(200);
    expect(navPollMs(0)).toBe(NAV_POLL_IDLE_MS);
    expect(NAV_POLL_IDLE_MS).toBe(1000);
  });

  it("carries the leased view on the one status path", () => {
    expect(navStatusPath(null)).toBe("/api/v1/nav/status");
    expect(navStatusPath("main:ar_route,depth")).toBe("/api/v1/nav/status?view=main%3Aar_route%2Cdepth");
  });
});

describe("dashboard video layer toggles", () => {
  it("offers depth and surfaces, off by default as layers.json defaults them", () => {
    expect(LEASED_LAYERS.map((l) => l.id)).toEqual(["depth", "surfaces"]);
    expect(defaultLeasedLayers()).toEqual({ depth: false, surfaces: false });
    expect(layersViewQuery(defaultLeasedLayers())).toBe("main:ar_route,ar_motion,obstacles");
  });

  it("leases a toggled-on layer through the same view", () => {
    expect(layersViewQuery({ depth: true, surfaces: false })).toBe("main:ar_route,ar_motion,obstacles,depth");
    expect(layersViewQuery({ depth: true, surfaces: true })).toBe("main:ar_route,ar_motion,obstacles,depth,surfaces");
  });
});

describe("owning skill detail", () => {
  const detail = { manifest: { gcs: { contributes: { skills: [{ id: "follow", state: { topic: "follow.state" } }] } } } };

  it("reads the skill's state topic from the manifest and its echoed parameters", () => {
    const topic = skillStateTopic(detail, "follow");
    expect(topic).toBe("follow.state");
    expect(skillDetailFromState({ "follow.state": { payload: { follow_mode: "orbit", orbit_radius_m: 8 } } }, topic)).toBe("ORBIT 8 m");
    expect(skillDetailFromState({ "follow.state": { payload: { follow_mode: "follow", follow_distance_m: 6.25 } } }, topic)).toBe("FOLLOW 6.3 m");
  });

  it("is null for an unknown skill or a missing topic", () => {
    expect(skillStateTopic(detail, "orbit")).toBeNull();
    expect(skillDetailFromState({}, "follow.state")).toBeNull();
  });
});
