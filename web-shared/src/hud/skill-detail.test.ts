import { describe, expect, it } from "vitest";

import cases from "./fixtures/skill-detail-cases.json";
import { skillDetail, skillDetailFromState, skillStateTopic } from "./skill-detail";

describe("skill detail golden cases (fixtures/skill-detail-cases.json)", () => {
  for (const c of cases) {
    it(c.name, () => {
      expect(skillDetail(c.input.echo as Record<string, unknown> | null)).toBe(c.expected);
    });
  }
});

describe("owning skill detail from the plugin state", () => {
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
