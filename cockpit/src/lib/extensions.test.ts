import { describe, expect, it } from "vitest";

import { parseExtensionDetail, parseSkillSettings, parseTargetActions, reportedStatesFor } from "@/lib/extensions";

const FOLLOW = "com.altnautica.follow-me";

describe("parseTargetActions", () => {
  it("parses entries ordered by order then id", () => {
    const out = parseTargetActions(FOLLOW, [
      { id: "zeta", order: 2 },
      { id: "follow", label: "Follow", order: 1, applies_to_class: "person", designate: true, config_key: "active", config_value: true },
      { id: "alpha", order: 2, designate: "yes" },
    ]);
    expect(out.map((a) => a.id)).toEqual(["follow", "alpha", "zeta"]);
    expect(out[0]).toEqual({
      pluginId: FOLLOW,
      id: "follow",
      label: "Follow",
      order: 1,
      appliesToClass: "person",
      designate: true,
      configKey: "active",
      configValue: true,
    });
    // Non-boolean designate is false; a missing label falls back to the id.
    expect(out[1]).toMatchObject({ designate: false, label: "alpha", appliesToClass: null, configKey: null });
  });

  it("skips entries without an id and tolerates a non-list", () => {
    expect(parseTargetActions(FOLLOW, [{ label: "x" }, "y", null])).toEqual([]);
    expect(parseTargetActions(FOLLOW, undefined)).toEqual([]);
    expect(parseTargetActions(FOLLOW, { id: "a" })).toEqual([]);
  });
});

describe("parseSkillSettings", () => {
  it("keeps plugin.config params with exactly two enum strings", () => {
    expect(
      parseSkillSettings([
        { key: "reacquire_mode", binding: "plugin.config", schema: { enum: ["strict", "smart"], default: "smart" } },
        { key: "speed", binding: "plugin.config", schema: { enum: ["a", "b", "c"] } },
        { key: "other", binding: "fc.param", schema: { enum: ["a", "b"] } },
      ]),
    ).toEqual([{ key: "reacquire_mode", values: ["strict", "smart"], defaultValue: "smart" }]);
  });
});

describe("parseExtensionDetail", () => {
  const detail = {
    granted_capabilities: ["ui.slot.video-overlay", 4],
    manifest: {
      name: "Follow Me",
      gcs: {
        entrypoint: "gcs/index.js",
        contributes: {
          panels: [{ id: "reticle", slot: "video.overlay" }, { id: "bad" }],
          skills: [
            { id: "follow", label: "Follow", toggle: true, confirm: true, activation: { config_key: "active" }, state: { topic: "follow.state" } },
            { id: "pulse", activation: { via: "rpc", config_key: "x" } },
          ],
          parameters: [{ key: "reacquire_mode", binding: "plugin.config", schema: { enum: ["strict", "smart"] } }],
          target_actions: [{ id: "follow", designate: true, config_key: "active", config_value: true }],
        },
      },
    },
  };

  it("builds skills with settings, panels and target actions", () => {
    const ext = parseExtensionDetail(FOLLOW, detail);
    expect(ext.name).toBe("Follow Me");
    expect(ext.grantedCapabilities).toEqual(["ui.slot.video-overlay"]);
    expect(ext.panels).toEqual([{ id: "reticle", slot: "video.overlay", title: "reticle", zone: null }]);
    expect(ext.skills).toHaveLength(1);
    expect(ext.skills[0]).toMatchObject({
      id: `${FOLLOW}:follow`,
      gesture: "hold",
      extension: { pluginId: FOLLOW, configKey: "active", toggle: true, stateTopic: "follow.state" },
      settings: [{ key: "reacquire_mode", values: ["strict", "smart"], defaultValue: null }],
    });
    expect(ext.targetActions).toHaveLength(1);
    expect(ext.targetActions[0]).toMatchObject({ pluginId: FOLLOW, designate: true, configKey: "active" });
  });

  it("reads the echoed setting value back from the state topic", () => {
    const ext = parseExtensionDetail(FOLLOW, detail);
    const states = reportedStatesFor(ext, {
      "follow.state": { payload: { active: true, commanding: true, reacquire_mode: "strict" }, ts_ms: 1 },
    });
    expect(states[`${FOLLOW}:follow`]).toEqual({ state: "active", echo: { reacquire_mode: "strict" } });
    expect(reportedStatesFor(ext, {})[`${FOLLOW}:follow`]).toEqual({ state: "idle" });
  });
});
