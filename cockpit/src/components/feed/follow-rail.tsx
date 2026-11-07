// The right rail (13-hud-v2 §8.2, L3): FOLLOW / TRACKING ×, the STRICT | SMART
// re-acquire toggle, the FRONT ⇄ BELLY camera swap and REC, then (expanded,
// tv) mirrors of the Skill Bar's HOLD, RTL and LAND. Every flight button
// dispatches the same skill through the same `requestSkill` gate as the Skill
// Bar, so it shares its disabled state, reason and confirm sheet. Kill is not
// here; it stays guarded in the safety band.

import { useState, type ReactNode } from "react";
import { ArrowLeftRight, Circle, Crosshair, Ellipsis, House, Pause, PlaneLanding, Square, X } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { SettingToggle } from "@/components/feed/reacquire-toggle";
import { Menu, type MenuItem } from "@/components/ui/menu";
import type { FollowSkill } from "@/hooks/use-follow-skill";
import { usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { startNodeRecording, startRecording, stopNodeRecording, stopRecording } from "@/lib/api";
import type { WidthClass } from "@/lib/cockpit-layout";
import { avoidChip, followButton, railButtonPx, railLayout, RAIL_GAP_PX, type RailButton } from "@/lib/follow-rail";
import { executeSkill, requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { toastFromError } from "@/shared/api/toast";
import type { FollowPhase } from "@/shared/hud/nav-types";
import { useProfile } from "@/shared/use-profile";
import { useConfirmStore } from "@/stores/confirm-store";
import { useFeedStore } from "@/stores/feed-store";
import { useSettingsStore } from "@/stores/settings-store";
import { useStatusStore } from "@/stores/status-store";

const MIRROR: Partial<Record<RailButton, { skillId: string; icon: LucideIcon; word: string }>> = {
  hold: { skillId: "pause", icon: Pause, word: "HOLD" },
  rth: { skillId: "rth", icon: House, word: "RTH" },
  land: { skillId: "land", icon: PlaneLanding, word: "LAND" },
};

function RailSlot({
  size,
  icon: Icon,
  word,
  label,
  onPress,
  disabled,
  active,
  danger,
}: {
  size: number;
  icon: LucideIcon;
  word: ReactNode;
  label: string;
  onPress: () => void;
  disabled?: boolean;
  active?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onPress}
      disabled={disabled}
      aria-label={label}
      aria-pressed={active}
      style={{ width: size, height: size }}
      className={cn(
        "flex shrink-0 flex-col items-center justify-center gap-[0.15rem] rounded-md border font-mono text-[9px] font-semibold uppercase leading-none tracking-[0.06em] disabled:opacity-45",
        danger
          ? "border-hud-crit/60 text-hud-crit"
          : active
            ? "border-hud-track bg-hud-track/20 text-hud-ink"
            : "border-hud-hair text-hud-ink hover:bg-hud-hair",
      )}
    >
      <Icon className="h-[1.05rem] w-[1.05rem]" aria-hidden />
      <span>{word}</span>
    </button>
  );
}

export function FollowRail({
  widthClass,
  safeHeightPx,
  follow,
  phase,
  designated,
  swapLabel,
}: {
  widthClass: WidthClass;
  safeHeightPx: number;
  follow: FollowSkill | null;
  phase: FollowPhase | null;
  designated: boolean;
  /** `FRONT ⇄ BELLY` from the two legs' labels, or null with no PiP leg. */
  swapLabel: string | null;
}) {
  const ctx = useSkillContext();
  const glove = useSettingsStore((s) => s.glove);
  const busy = useConfirmStore((s) => s.busy);
  const swapPip = useFeedStore((s) => s.swapPip);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const groundStation = useProfile() === "ground_station";
  const [recBusy, setRecBusy] = useState(false);
  const [foldAnchor, setFoldAnchor] = useState<DOMRect | null>(null);

  const modeSetting = follow?.skill.settings?.find((s) => s.key === "reacquire_mode") ?? null;
  const patternSetting = follow?.skill.settings?.find((s) => s.key === "follow_mode") ?? null;
  const avoid = avoidChip(usePilot()?.data.avoidState ?? null);
  const layout = railLayout({
    width: widthClass,
    followInstalled: follow !== null,
    hasPattern: patternSetting !== null,
    hasModeSetting: modeSetting !== null,
    canSwap: swapLabel !== null,
    safeHeightPx,
  });
  const size = railButtonPx(widthClass, glove);

  const toggleRec = async () => {
    if (recBusy) return;
    setRecBusy(true);
    try {
      // A ground station records with its own recorder; every other node
      // records its own video legs.
      if (groundStation) await (recording ? stopRecording() : startRecording());
      else await (recording ? stopNodeRecording() : startNodeRecording());
    } catch (e) {
      toastFromError(e, recording ? "Recording did not stop" : "Recording did not start");
    } finally {
      setRecBusy(false);
    }
  };


  const render = (b: RailButton): ReactNode => {
    switch (b) {
      case "follow": {
        if (!follow) return null;
        const word = followButton(follow.state.active === true, phase, designated);
        if (word === null) return null;
        if (word === "tracking") {
          // Stop is a tap: stopping makes the vehicle hold.
          return (
            <RailSlot
              key={b}
              size={size}
              icon={X}
              word="TRACKING ×"
              label="Stop tracking"
              active
              disabled={busy}
              onPress={() => void executeSkill(follow.skill, { active: true })}
            />
          );
        }
        return (
          <RailSlot
            key={b}
            size={size}
            icon={Crosshair}
            word="FOLLOW"
            label={follow.state.enabled ? "Follow target" : `Follow: ${follow.state.reason ?? "unavailable"}`}
            disabled={!follow.state.enabled || busy}
            onPress={() => requestSkill(follow.skill, follow.ctx)}
          />
        );
      }
      case "mode":
        return follow && modeSetting ? (
          <SettingToggle key={b} skill={follow.skill} setting={modeSetting} vertical className="bg-hud-glass" />
        ) : null;
      case "pattern":
        return follow && patternSetting ? (
          <div key={b} className="flex flex-col items-stretch gap-[0.2rem]">
            <SettingToggle skill={follow.skill} setting={patternSetting} vertical className="bg-hud-glass" />
            {avoid ? (
              <span
                data-testid="avoid-chip"
                className={cn(
                  "rounded border px-[0.25rem] text-center font-mono text-[0.68rem]",
                  avoid.tone === "ok" ? "border-ok/60 text-ok" : avoid.tone === "warn" ? "border-warn text-warn" : "border-err text-err",
                )}
              >
                {avoid.text}
              </span>
            ) : null}
          </div>
        ) : null;
      case "camSwap":
        return (
          <RailSlot key={b} size={size} icon={ArrowLeftRight} word={swapLabel ?? ""} label={`Swap cameras ${swapLabel ?? ""}`} onPress={swapPip} />
        );
      case "rec":
        return (
          <RailSlot
            key={b}
            size={size}
            icon={recording ? Square : Circle}
            word="REC"
            label={recording ? "Stop recording" : "Start recording"}
            active={recording}
            disabled={recBusy}
            onPress={() => void toggleRec()}
          />
        );
      default: {
        const m = MIRROR[b];
        const skill = m ? BUILTIN_SKILL_BY_ID[m.skillId] : undefined;
        if (!m || !skill) return null;
        const st = resolveSkillState(skill, ctx);
        return (
          <RailSlot
            key={b}
            size={size}
            icon={m.icon}
            word={m.word}
            label={st.enabled ? skill.label : `${skill.label}: ${st.reason ?? "unavailable"}`}
            disabled={!st.enabled || busy}
            onPress={() => requestSkill(skill, ctx)}
          />
        );
      }
    }
  };

  const head = layout.buttons.filter((b) => !MIRROR[b]);
  const tail = layout.buttons.filter((b) => MIRROR[b]);
  const foldItems: MenuItem[] = layout.folded.map((b) =>
    b === "camSwap"
      ? { label: `Swap cameras ${swapLabel ?? ""}`, icon: ArrowLeftRight, onSelect: swapPip }
      : { label: recording ? "Stop recording" : "Start recording", icon: recording ? Square : Circle, enabled: !recBusy, onSelect: () => void toggleRec() },
  );

  return (
    <div
      className="pointer-events-auto flex flex-col items-stretch rounded-lg border border-hud-hair bg-hud-glass p-[5px] backdrop-blur-hud"
      style={{ gap: RAIL_GAP_PX, width: size + 12 }}
      aria-label="Follow controls"
      role="toolbar"
      aria-orientation="vertical"
    >
      {head.map(render)}
      {layout.folded.length > 0 ? (
        <RailSlot
          size={size}
          icon={Ellipsis}
          word="MORE"
          label="More controls"
          onPress={() => setFoldAnchor(document.activeElement?.getBoundingClientRect() ?? null)}
        />
      ) : null}
      {layout.divider && tail.length > 0 ? <div className="mx-[4px] h-px bg-hud-hair" aria-hidden /> : null}
      {tail.map(render)}
      {foldAnchor ? <Menu anchor={foldAnchor} items={foldItems} onClose={() => setFoldAnchor(null)} /> : null}
    </div>
  );
}
