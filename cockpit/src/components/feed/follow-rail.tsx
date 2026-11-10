// The follow controls (L3), a row in the follow strip under the top bar:
// FOLLOW / the tracking close chip, the FOLLOW | ORBIT pattern with the avoidance chip,
// the STRICT | SMART re-acquire toggle, the FRONT ⇄ BELLY camera swap and
// REC. Hold, return home and land are the left column's alone; the rail never
// repeats them. Kill is not here; it stays guarded in the top bar.

import { useState, type ReactNode } from "react";
import { ArrowLeftRight, Circle, Crosshair, Ellipsis, Square, X } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { SettingToggle } from "@/components/feed/reacquire-toggle";
import { Menu, type MenuItem } from "@/components/ui/menu";
import type { FollowSkill } from "@/hooks/use-follow-skill";
import { usePilot } from "@/hooks/use-pilot";
import { startNodeRecording, startRecording, stopNodeRecording, stopRecording } from "@/lib/api";
import type { WidthClass } from "@/lib/cockpit-layout";
import { avoidChip, followButton, railButtonPx, railLayout, RAIL_GAP_PX, type RailButton } from "@/lib/follow-rail";
import { FLIGHT_PX } from "@/lib/flight-layout";
import { executeSkill, requestSkill, useSkillBusy } from "@/lib/skill-runner";
import { cn } from "@/lib/utils";
import { toastFromError } from "@/shared/api/toast";
import { specText } from "@/shared/hud/spec-text";
import type { FollowPhase } from "@/shared/hud/nav-types";
import { useProfile } from "@/shared/use-profile";
import { useFeedStore } from "@/stores/feed-store";
import { useSettingsStore } from "@/stores/settings-store";
import { useStatusStore } from "@/stores/status-store";

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
        "flex shrink-0 flex-col items-center justify-center gap-[2px] rounded-md border font-mono text-[9px] font-semibold uppercase leading-none tracking-[0.06em] disabled:opacity-45",
        danger
          ? "border-hud-crit/60 text-hud-crit"
          : active
            ? "border-hud-track bg-hud-track/20 text-hud-ink"
            : "border-hud-hair text-hud-ink hover:bg-hud-hair",
      )}
    >
      <Icon className="h-[17px] w-[17px]" aria-hidden />
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
  const glove = useSettingsStore((s) => s.glove);
  const busy = useSkillBusy(follow?.skill);
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
  // One row in the follow strip: the buttons fit its height.
  const size = Math.min(railButtonPx(widthClass, glove), FLIGHT_PX.stripH - 12);

  const toggleRec = async () => {
    if (recBusy) return;
    setRecBusy(true);
    try {
      // A ground station records with its own recorder; every other node
      // records its own video legs.
      if (groundStation) await (recording ? stopRecording() : startRecording());
      else await (recording ? stopNodeRecording() : startNodeRecording());
    } catch (e) {
      toastFromError(e, specText(recording ? "follow.rec_stop_failed" : "follow.rec_start_failed"));
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
          // Stop is a tap: stopping makes the vehicle hold, so it never waits
          // behind the start that is still awaiting its answer.
          return (
            <RailSlot
              key={b}
              size={size}
              icon={X}
              word={specText("follow.tracking_close")}
              label={specText("follow.stop_tracking")}
              active
              onPress={() => void executeSkill(follow.skill, { active: true })}
            />
          );
        }
        return (
          <RailSlot
            key={b}
            size={size}
            icon={Crosshair}
            word={specText("follow.chip")}
            label={follow.state.enabled ? specText("follow.target") : `${specText("follow.target")}: ${follow.state.reason ?? ""}`}
            disabled={!follow.state.enabled || busy}
            onPress={() => requestSkill(follow.skill, follow.ctx)}
          />
        );
      }
      case "mode":
        return follow && modeSetting ? (
          <SettingToggle key={b} skill={follow.skill} setting={modeSetting} className="h-[44px] shrink-0 bg-white/5" />
        ) : null;
      case "pattern":
        return follow && patternSetting ? (
          <div key={b} className="flex shrink-0 items-center gap-[6px]">
            <SettingToggle skill={follow.skill} setting={patternSetting} className="h-[44px] bg-white/5" />
            {avoid ? (
              <span
                data-testid="avoid-chip"
                className={cn(
                  "rounded border px-[4px] text-center font-mono text-[11px]",
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
          <RailSlot key={b} size={size} icon={ArrowLeftRight} word={swapLabel ?? ""} label={specText("follow.swap_cameras", { pair: swapLabel ?? "" })} onPress={swapPip} />
        );
      case "rec":
        return (
          <RailSlot
            key={b}
            size={size}
            icon={recording ? Square : Circle}
            word={specText("follow.rec")}
            label={specText(recording ? "cockpit.camera.stop_recording" : "cockpit.camera.start_recording")}
            active={recording}
            disabled={recBusy}
            onPress={() => void toggleRec()}
          />
        );
      default:
        return null;
    }
  };

  const foldItems: MenuItem[] = layout.folded.map((b) =>
    b === "camSwap"
      ? { label: specText("follow.swap_cameras", { pair: swapLabel ?? "" }), icon: ArrowLeftRight, onSelect: swapPip }
      : {
          label: specText(recording ? "cockpit.camera.stop_recording" : "cockpit.camera.start_recording"),
          icon: recording ? Square : Circle,
          enabled: !recBusy,
          onSelect: () => void toggleRec(),
        },
  );

  return (
    <div
      className="pointer-events-auto flex min-w-0 flex-row items-center overflow-hidden"
      style={{ gap: RAIL_GAP_PX }}
      aria-label={specText("follow.controls")}
      role="toolbar"
      aria-orientation="horizontal"
    >
      {layout.buttons.map(render)}
      {layout.folded.length > 0 ? (
        <RailSlot
          size={size}
          icon={Ellipsis}
          word={specText("follow.more")}
          label={specText("follow.more_controls")}
          onPress={() => setFoldAnchor(document.activeElement?.getBoundingClientRect() ?? null)}
        />
      ) : null}
      {foldAnchor ? <Menu anchor={foldAnchor} items={foldItems} onClose={() => setFoldAnchor(null)} /> : null}
    </div>
  );
}
