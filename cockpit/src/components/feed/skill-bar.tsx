// The Feed's Skill Bar: the built-in flight skills the agent can drive through
// `POST /api/command`, the FC family's mode presets, and the running
// extensions' skills after them. Each slot shows its disabled reason as text
// under the button (a tooltip is unreachable on a touch panel), every non-tap
// skill opens the shared confirm sheet, and Kill is always present behind its
// guard.

import { memo, useState } from "react";
import {
  ChevronLeft,
  CircleDot,
  Gauge,
  Home,
  OctagonX,
  Pause,
  PlaneLanding,
  PlaneTakeoff,
  Play,
  Power,
  PowerOff,
  Puzzle,
  Sliders,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { useSkillContext } from "@/hooks/use-skill-context";
import { requestSkill } from "@/lib/skill-runner";
import {
  CORE_BY_ID,
  modePresetsFor,
  resolveSkillState,
  type Skill,
  type SkillContext,
  type SkillState,
} from "@/lib/skills";
import { cn } from "@/lib/utils";
import { useConfirmStore } from "@/stores/confirm-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";

const CORE_ICONS: Record<string, LucideIcon> = {
  arm: Power,
  disarm: PowerOff,
  takeoff: PlaneTakeoff,
  land: PlaneLanding,
  rtl: Home,
  pause: Pause,
  resume: Play,
  kill: OctagonX,
};

const SkillSlot = memo(function SkillSlot({
  skill,
  icon: Icon,
  state,
  busy,
  onPress,
}: {
  skill: Skill;
  icon: LucideIcon;
  state: SkillState;
  busy: boolean;
  onPress: (skill: Skill) => void;
}) {
  const danger = skill.gesture === "guarded";
  // Kill stays available while another command awaits its acknowledgement.
  const enabled = state.enabled && (!busy || danger);
  return (
    <div className="flex w-[4.4rem] flex-col items-center gap-[0.15rem]">
      <button
        type="button"
        onClick={() => onPress(skill)}
        disabled={!enabled}
        aria-label={state.enabled ? skill.label : `${skill.label}: ${state.reason ?? "unavailable"}`}
        aria-pressed={skill.extension?.toggle ? state.active === true : undefined}
        className={cn(
          "relative flex min-h-[max(3.2rem,48px)] w-full flex-col items-center justify-center gap-[0.15rem] rounded-lg border px-[0.3rem] backdrop-blur-hud transition-colors duration-quick disabled:opacity-45",
          danger
            ? "border-err/60 bg-err/15 text-err"
            : state.active
              ? "border-primary bg-primary/25 text-hud-ink"
              : "border-hud-hair bg-hud-glass text-hud-ink hover:bg-hud-glass-strong",
        )}
      >
        <Icon className="h-[1.35rem] w-[1.35rem]" aria-hidden />
        <span className="text-[0.75rem] font-medium leading-none">{skill.label}</span>
        {state.badge ? (
          <span className="absolute right-[0.2rem] top-[0.15rem] rounded bg-primary px-[0.2rem] font-mono text-[0.75rem] leading-none text-primary-foreground">
            {state.badge}
          </span>
        ) : null}
      </button>
      {!state.enabled && state.reason ? (
        <span className="w-full truncate text-center text-[0.75rem] leading-tight text-hud-ink-2">
          {state.reason}
        </span>
      ) : null}
    </div>
  );
});

function slots(skills: Skill[], ctx: SkillContext, busy: boolean, onPress: (s: Skill) => void) {
  return skills.map((skill) => (
    <SkillSlot
      key={skill.id}
      skill={skill}
      icon={
        CORE_ICONS[skill.id] ??
        (skill.category === "mode" ? Gauge : skill.extension ? Puzzle : CircleDot)
      }
      state={resolveSkillState(skill, ctx)}
      busy={busy}
      onPress={onPress}
    />
  ));
}

export function SkillBar() {
  const ctx = useSkillContext();
  const autopilot = useFlightStore((s) => s.telemetry?.autopilot ?? null);
  const busy = useConfirmStore((s) => s.busy);
  const ack = useConfirmStore((s) => s.ack);
  const extensions = useExtensionsStore((s) => s.extensions);
  const [page, setPage] = useState<"primary" | "mode">("primary");

  const onPress = (skill: Skill) => {
    if (requestSkill(skill, ctx) && skill.category === "mode") setPage("primary");
  };

  const primary = [
    CORE_BY_ID[ctx.armed ? "disarm" : "arm"],
    CORE_BY_ID.takeoff,
    CORE_BY_ID.land,
    CORE_BY_ID.rtl,
    CORE_BY_ID.pause,
    CORE_BY_ID.resume,
  ];
  const extensionSkills = extensions.flatMap((e) => e.skills);
  const modeGate = resolveSkillState(modePresetsFor(autopilot)[0], ctx);

  return (
    <div className="flex min-w-0 flex-1 justify-start">
      <div className="pointer-events-auto flex min-w-0 max-w-full flex-col items-start gap-[0.35rem]">
        {ack ? (
          <div
            role="status"
            className={cn(
              "max-w-[22rem] truncate rounded-md bg-hud-glass-strong px-[0.5rem] py-[0.25rem] font-mono text-[0.75rem] backdrop-blur-hud",
              ack.kind === "ok" && "text-ok",
              ack.kind === "warn" && "text-warn",
              ack.kind === "err" && "text-err",
            )}
          >
            {ack.text}
          </div>
        ) : null}

        <div className="flex max-w-full items-start gap-[0.35rem] overflow-x-auto">
          {page === "mode" ? (
            <>
              <div className="flex w-[4.4rem] flex-col items-center">
                <button
                  type="button"
                  onClick={() => setPage("primary")}
                  className="flex min-h-[max(3.2rem,48px)] w-full flex-col items-center justify-center gap-[0.15rem] rounded-lg border border-hud-hair bg-hud-glass text-hud-ink backdrop-blur-hud"
                >
                  <ChevronLeft className="h-[1.35rem] w-[1.35rem]" aria-hidden />
                  <span className="text-[0.75rem] font-medium leading-none">Back</span>
                </button>
              </div>
              {slots(modePresetsFor(autopilot), ctx, busy, onPress)}
            </>
          ) : (
            <>
              {slots(primary, ctx, busy, onPress)}
              <div className="flex w-[4.4rem] flex-col items-center gap-[0.15rem]">
                <button
                  type="button"
                  onClick={() => setPage("mode")}
                  disabled={!modeGate.enabled || busy}
                  aria-label={modeGate.enabled ? "Mode" : `Mode: ${modeGate.reason ?? "unavailable"}`}
                  className="flex min-h-[max(3.2rem,48px)] w-full flex-col items-center justify-center gap-[0.15rem] rounded-lg border border-hud-hair bg-hud-glass text-hud-ink backdrop-blur-hud disabled:opacity-45"
                >
                  <Sliders className="h-[1.35rem] w-[1.35rem]" aria-hidden />
                  <span className="text-[0.75rem] font-medium leading-none">Mode</span>
                </button>
                {!modeGate.enabled && modeGate.reason ? (
                  <span className="w-full truncate text-center text-[0.75rem] leading-tight text-hud-ink-2">
                    {modeGate.reason}
                  </span>
                ) : null}
              </div>
              {slots(extensionSkills, ctx, busy, onPress)}
              {slots([CORE_BY_ID.kill], ctx, busy, onPress)}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
