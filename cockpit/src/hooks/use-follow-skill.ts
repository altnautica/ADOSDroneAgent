// The follow-type skill the Feed's follow chrome drives (the toggle skill a
// designating target action switches on), and the chip detail of whichever
// motion skill owns the aircraft now.

import { usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { skillDetail } from "@/shared/hud/skill-detail";
import { resolveSkillState, type Skill, type SkillContext, type SkillState } from "@/lib/skills";
import { useExtensionsStore } from "@/stores/extensions-store";

export interface FollowSkill {
  skill: Skill;
  state: SkillState;
  ctx: SkillContext;
}

export function useFollowSkill(): FollowSkill | null {
  const extensions = useExtensionsStore((s) => s.extensions);
  const ctx = useSkillContext();
  for (const ext of extensions) {
    for (const action of ext.targetActions) {
      if (!action.designate || !action.configKey) continue;
      const skill = ext.skills.find((s) => s.extension?.toggle && s.extension.configKey === action.configKey);
      if (skill) return { skill, state: resolveSkillState(skill, ctx), ctx };
    }
  }
  return null;
}

/** The flight-state chip's detail for the skill the pilot reports as owner
 *  (`ORBIT 8 m`), from that skill's echoed parameters; null when unknown. */
export function useActiveSkillDetail(): string | null {
  const owner = usePilot()?.data.skill ?? null;
  const echo = useExtensionsStore((s) => (owner ? s.reported[`${owner.pluginId}:${owner.skillId}`]?.echo : undefined));
  return skillDetail(echo);
}
