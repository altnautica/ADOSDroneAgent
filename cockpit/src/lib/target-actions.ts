// Running what the operator picks on a target: Lock (designate the box on the
// agent), Release (drop the host designation and stop a running follow), and
// manifest-declared plugin target actions (designate, then write the plugin
// key). A plugin action that turns a toggle skill on goes through that skill's
// own gate and confirm tier (`requestSkill`), exactly like a skill picker tap;
// turning one off is a tap, because stopping makes the vehicle hold.

import { designateTarget, putPluginConfig } from "@/lib/api";
import type { TargetAction } from "@/lib/extensions";
import { executeSkill, requestSkill } from "@/lib/skill-runner";
import { resolveSkillState, type Skill, type SkillContext } from "@/lib/skills";
import { toast, toastFromError } from "@/shared/api/toast";
import type { DesignatedRef, TargetBox } from "@/shared/hud/target-state";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** The toggle skill a plugin drives through `configKey`, if any. */
export function skillForConfigKey(pluginId: string, configKey: string | null): Skill | null {
  if (!configKey) return null;
  for (const ext of useExtensionsStore.getState().extensions) {
    if (ext.pluginId !== pluginId) continue;
    const skill = ext.skills.find((s) => s.extension?.configKey === configKey);
    if (skill) return skill;
  }
  return null;
}

/** Designate `bbox` on `cameraId` and mirror it in the host store. Returns
 *  false (with an error toast) when the agent refused. */
export async function lockTarget(
  cameraId: string,
  bbox: TargetBox,
  opts: { classLabel?: string; confidence?: number; trackId?: number | null } = {},
): Promise<boolean> {
  try {
    const res = await designateTarget(cameraId, bbox, opts);
    if (!res.designated) {
      toast.err("Target not locked", "The vision engine did not accept the box.");
      return false;
    }
    const ref: DesignatedRef = { cameraId, trackId: res.track_id ?? opts.trackId ?? null, bbox };
    useNavStreamStore.getState().designate(ref);
    return true;
  } catch (e) {
    toastFromError(e, "Target not locked");
    return false;
  }
}

/** Release the target: clear the host designation and stop any running
 *  follow-type toggle skill (a tap, never a held confirm). */
export async function releaseTarget(ctx: SkillContext): Promise<void> {
  useNavStreamStore.getState().designate(null);
  for (const ext of useExtensionsStore.getState().extensions) {
    for (const action of ext.targetActions) {
      if (!action.designate) continue;
      const skill = skillForConfigKey(action.pluginId, action.configKey);
      if (skill && resolveSkillState(skill, ctx).active) await executeSkill(skill, { active: true });
    }
  }
}

/** Run a plugin target action on a picked box. */
export async function runTargetAction(
  action: TargetAction,
  target: { cameraId: string; bbox: TargetBox; classLabel: string; confidence: number; trackId: number | null },
  ctx: SkillContext,
): Promise<void> {
  if (action.designate) {
    const ok = await lockTarget(target.cameraId, target.bbox, target);
    if (!ok) return;
  }
  if (!action.configKey) return;
  const skill = skillForConfigKey(action.pluginId, action.configKey);
  if (skill?.extension?.toggle) {
    const state = resolveSkillState(skill, ctx);
    const wantOn = action.configValue === true;
    if (wantOn && !state.active) requestSkill(skill, ctx);
    else if (!wantOn && state.active) await executeSkill(skill, { active: true });
    return;
  }
  try {
    await putPluginConfig(action.pluginId, action.configKey, action.configValue);
  } catch (e) {
    toastFromError(e, `${action.label} failed`);
  }
}
