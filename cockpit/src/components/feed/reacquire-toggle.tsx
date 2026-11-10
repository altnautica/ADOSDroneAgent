// A toggle skill's two-value quick setting as a segmented control (follow-me's
// STRICT | SMART). A written value awaiting the plugin's echo carries a 1 px
// dashed outline (13-hud-v2 §8.2 #2); a tap is the whole confirm.

import { specText } from "@/shared/hud/spec-text";
import { useSkillSetting } from "@/hooks/use-skill-setting";
import type { Skill, SkillSetting } from "@/lib/skills";
import { cn } from "@/lib/utils";

export function SettingToggle({
  skill,
  setting,
  vertical = false,
  className,
}: {
  skill: Skill;
  setting: SkillSetting;
  vertical?: boolean;
  className?: string;
}) {
  const s = useSkillSetting(skill, setting);
  return (
    <div
      role="radiogroup"
      aria-label={setting.key === "reacquire_mode" ? specText("target.reacquire_mode") : setting.key}
      className={cn("flex overflow-hidden rounded-md border border-hud-hair", vertical ? "flex-col" : "flex-row", className)}
    >
      {setting.values.map((v) => {
        const on = s.value === v;
        return (
          <button
            key={v}
            type="button"
            role="radio"
            aria-checked={on}
            onClick={() => s.set(v)}
            className={cn(
              "min-h-[24px] flex-1 px-[6px] font-mono text-[11px] font-semibold uppercase tracking-[0.06em]",
              on ? "bg-hud-track/25 text-hud-ink" : "text-hud-ink-2 hover:bg-hud-hair",
              on && s.pending && "outline-dashed outline-1 -outline-offset-2 outline-hud-ink",
            )}
          >
            {v}
          </button>
        );
      })}
    </div>
  );
}
