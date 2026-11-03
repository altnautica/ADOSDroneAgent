// The follow skill as the Feed's chrome sees it: the toggle skill an installed
// plugin turns on from a designating target action (follow-me's `active`),
// with its gate state. Null when no follow-type plugin is running.

import { useSkillContext } from "@/hooks/use-skill-context";
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
