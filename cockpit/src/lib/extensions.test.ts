import { describe, expect, it } from "vitest";

import { parseExtensionDetail, parseExtensionSkill, parseTargetActions } from "@/lib/extensions";

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

const SIYI = "com.altnautica.siyi-pod";

function skill(id: string, over: Record<string, unknown> = {}) {
  return {
    id,
    label: `skill.${id}`,
    icon: "camera",
    category: "camera",
    toggle: false,
    confirm: false,
    arm_requirement: "any",
    activation: { via: "config", config_key: id.replace(/-/g, "_") },
    state: { via: "event", topic: "siyi.pod.state" },
    ...over,
  };
}

describe("parseExtensionSkill (C5 strict schema)", () => {
  const ctx = { pluginName: "SIYI pod" };

  it("accepts a valid skill with its category, motion and binding fields", () => {
    const r = parseExtensionSkill(FOLLOW, skill("follow-me", { category: "behavior", toggle: true, confirm: true, arm_requirement: "armed", motion: true, nudge: true, vehicle_classes: ["multirotor", "vtol"], default_binding: { key: "shift+f" } }), { pluginName: "Follow" });
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    expect(r.skill.category).toBe("behavior");
    expect(r.skill.extension).toMatchObject({ motion: true, nudge: true, vehicleClasses: ["multirotor", "vtol"], toggle: true });
    expect(r.skill.confirm.kind).toBe("hold");
  });

  it("rejects a skill with no state.via, an unknown category or a non-config activation", () => {
    const { state: _s, ...noState } = skill("photo");
    void _s;
    for (const bad of [noState, skill("photo", { state: { topic: "t" } }), skill("photo", { category: "flight" }), skill("photo", { activation: { via: "method", config_key: "x" } }), skill("Photo_Bad")]) {
      expect(parseExtensionSkill(SIYI, bad, ctx).ok).toBe(false);
    }
  });

  it("keeps all eight skills of a plugin that declares eight", () => {
    const ids = ["siyi-record", "cycle-palette", "fire-lrf", "center", "nadir", "zoom-in", "zoom-out", "photo"];
    const detail = { manifest: { name: "SIYI pod", gcs: { contributes: { skills: ids.map((id) => skill(id, { toggle: id === "siyi-record" })) } } } };
    const ext = parseExtensionDetail(SIYI, detail);
    expect(ext.skills.map((s) => s.id)).toEqual(ids.map((id) => `${SIYI}:${id}`));
    expect(ext.rejected).toEqual([]);
    // i18n-key labels never render raw.
    expect(ext.skills.every((s) => !s.label.startsWith("skill."))).toBe(true);
  });
});
