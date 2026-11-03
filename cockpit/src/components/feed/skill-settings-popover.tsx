// A toggle skill's quick-settings popover from the Skill Bar (13-hud-v2 §8.2):
// each two-value plugin setting as a segmented toggle (follow-me's re-acquire
// STRICT | SMART), and, for the follow skill, its live phase. Its own lazy
// chunk; it opens only on demand.

import { SettingToggle } from "@/components/feed/reacquire-toggle";
import { Overlay } from "@/components/ui/overlay";
import { useNow } from "@/hooks/use-now";
import { placeMenu, type AnchorRect } from "@/lib/menu-placement";
import type { Skill } from "@/lib/skills";
import { FOLLOW_FRESH_MS } from "@/shared/hud/target-state";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const SETTING_TITLE: Record<string, string> = { reacquire_mode: "Re-acquire" };

export default function SkillSettingsPopover({
  skill,
  anchor,
  showPhase,
  onClose,
}: {
  skill: Skill;
  anchor: AnchorRect | null;
  /** The skill is the follow skill: show follow-me's phase. */
  showPhase: boolean;
  onClose: () => void;
}) {
  const follow = useNavStreamStore((s) => s.follow);
  const now = useNow(showPhase && follow ? 1000 : null);
  const place = placeMenu(anchor, window.innerWidth, window.innerHeight);
  const phase =
    showPhase && follow ? (now - follow.at <= FOLLOW_FRESH_MS ? follow.data.phase.toUpperCase() : "STALE") : null;
  return (
    <Overlay
      onDismiss={onClose}
      scrim="clear"
      labelledBy="skill-settings-title"
      panelClassName="absolute flex min-w-[12rem] flex-col gap-[0.5rem] rounded-lg border border-border bg-surface p-[0.6rem] text-surface-foreground shadow-lg"
      panelStyle={{ left: place.left, top: place.top, bottom: place.bottom }}
    >
      <div className="flex items-baseline justify-between gap-[0.6rem]">
        <span id="skill-settings-title" className="text-[0.85rem] font-semibold">
          {skill.label}
        </span>
        {phase ? <span className="font-mono text-[0.7rem] text-muted-foreground">{phase}</span> : null}
      </div>
      {(skill.settings ?? []).map((s) => (
        <div key={s.key} className="flex flex-col gap-[0.25rem]">
          <span className="text-[0.75rem] text-muted-foreground">{SETTING_TITLE[s.key] ?? s.key}</span>
          <SettingToggle skill={skill} setting={s} className="bg-hud-glass" />
        </div>
      ))}
    </Overlay>
  );
}
