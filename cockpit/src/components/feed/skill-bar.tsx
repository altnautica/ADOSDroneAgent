// The Feed's flight-command dock — a compact bottom-left bar of the flight
// actions the agent can actually drive through `POST /api/command`: arm/disarm,
// takeoff, land, RTL, and the FC family's flight-mode presets. It sits apart from
// the centre utility action bar (Back/Menu/Record/…) and shares the same touch
// look.
//
// Honest boundary (Rule 44): the ONLY control path here is the fixed set of
// MAVLink COMMAND_LONG actions — there is no virtual-stick / MANUAL_CONTROL and no
// guided goto over this REST surface, so the bar contains only commands the agent
// can genuinely execute. A skill the node cannot drive right now (no live FC link)
// renders disabled with a plain reason; the high-consequence actions are guarded
// behind an explicit confirm; and the result of every send is reported truthfully
// (accepted / rejected with the FC's reason / sent-but-not-acknowledged / failed),
// never an optimistic success.

import { useEffect, useState } from "react";
import {
  Check,
  ChevronLeft,
  Gauge,
  Home,
  PlaneLanding,
  PlaneTakeoff,
  Power,
  PowerOff,
  Sliders,
  X,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import { ApiError, sendCommand } from "@/lib/api";
import {
  CORE_SKILLS,
  modePresetsFor,
  resolveSkillState,
  type Skill,
  type SkillContext,
} from "@/lib/skills";
import { cn } from "@/lib/utils";

/** Icon per core skill id. Mode presets share one icon (the mode gauge). */
const CORE_ICONS: Record<string, LucideIcon> = {
  arm: Power,
  disarm: PowerOff,
  takeoff: PlaneTakeoff,
  land: PlaneLanding,
  rtl: Home,
};

const CORE_BY_ID: Record<string, Skill> = Object.fromEntries(
  CORE_SKILLS.map((s) => [s.id, s]),
);

type AckKind = "ok" | "warn" | "err";
interface AckLine {
  text: string;
  kind: AckKind;
}

/** A single flight-command button, matching the utility action bar's look. High-
 *  consequence actions carry a subtle warning tint; a disabled button shows its
 *  reason as the tooltip and does not intercept taps. */
function SkillButton({
  icon: Icon,
  label,
  onClick,
  enabled,
  reason,
  emphasis,
}: {
  icon: LucideIcon;
  label: string;
  onClick: () => void;
  enabled: boolean;
  reason?: string;
  emphasis?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!enabled}
      aria-label={label}
      title={enabled ? label : reason}
      className={cn(
        "flex min-h-[max(3rem,48px)] min-w-[max(3rem,48px)] flex-col items-center justify-center gap-[0.15rem] rounded-lg px-[0.35rem] backdrop-blur-sm transition-colors disabled:opacity-40",
        emphasis
          ? "bg-background/55 text-surface-foreground ring-1 ring-warn/40 hover:bg-muted active:bg-muted"
          : "bg-background/55 text-surface-foreground hover:bg-muted active:bg-muted",
      )}
    >
      <Icon className="h-[1.35rem] w-[1.35rem]" aria-hidden />
      <span className="text-[0.58rem] font-medium leading-none">{label}</span>
    </button>
  );
}

export function SkillBar() {
  const { telemetry, live } = useFlightTelemetryContext();

  const fcConnected = live;
  const armed = telemetry?.armed === true;
  const autopilot = telemetry?.autopilot ?? null;
  const ctx: SkillContext = { fcConnected, armed };

  const [page, setPage] = useState<"primary" | "mode">("primary");
  const [pending, setPending] = useState<Skill | null>(null);
  const [busy, setBusy] = useState(false);
  const [ack, setAck] = useState<AckLine | null>(null);

  // Clear a transient ACK line after a few seconds.
  useEffect(() => {
    if (!ack) return;
    const id = setTimeout(() => setAck(null), 4000);
    return () => clearTimeout(id);
  }, [ack]);

  const run = async (skill: Skill) => {
    if (busy) return;
    setPending(null);
    setBusy(true);
    try {
      const res = await sendCommand(skill.cmd, skill.args);
      const a = res.ack;
      if (a?.observed) {
        if (a.accepted) {
          setAck({ text: `${skill.label}: accepted`, kind: "ok" });
        } else {
          const why = a.statustext ? ` — ${a.statustext}` : "";
          setAck({
            text: `${skill.label}: ${a.result_name ?? "rejected"}${why}`,
            kind: "err",
          });
        }
      } else {
        // Sent, but the FC did not acknowledge within the window (honest — not a
        // fabricated success).
        setAck({ text: `${skill.label}: sent, no acknowledgement`, kind: "warn" });
      }
    } catch (e) {
      setAck({ text: `${skill.label}: ${errText(e)}`, kind: "err" });
    } finally {
      setBusy(false);
      // A mode change returns to the primary page so the dock is ready to fly.
      if (skill.category === "mode") setPage("primary");
    }
  };

  const onSkill = (skill: Skill, state: { enabled: boolean }) => {
    if (!state.enabled || busy) return;
    if (skill.confirm) {
      setPending(skill);
    } else {
      void run(skill);
    }
  };

  // The armed-state-appropriate core action (arm when disarmed, disarm when
  // armed): only the applicable one is offered, and the gating disables it when
  // there is no FC link.
  const armSkill = CORE_BY_ID[armed ? "disarm" : "arm"];
  const primarySkills = [
    armSkill,
    CORE_BY_ID.takeoff,
    CORE_BY_ID.land,
    CORE_BY_ID.rtl,
  ];
  const modeState = resolveSkillState(
    { id: "mode-open", label: "Mode", cmd: "mode", args: [], confirm: false, category: "mode" },
    ctx,
  );

  const modeSkills = modePresetsFor(autopilot);

  return (
    <div className="pointer-events-none absolute bottom-[0.6rem] left-[0.5rem] z-20">
      <div className="pointer-events-auto flex flex-col items-start gap-[0.35rem]">
        {/* transient command-result line (accepted / rejected / no-ack / error) */}
        {ack ? (
          <div
            className={cn(
              "max-w-[18rem] truncate rounded-md bg-background/75 px-[0.5rem] py-[0.25rem] font-mono text-[0.62rem] backdrop-blur-sm",
              ack.kind === "ok" && "text-ok",
              ack.kind === "warn" && "text-warn",
              ack.kind === "err" && "text-err",
            )}
            role="status"
          >
            {ack.text}
          </div>
        ) : null}

        {pending ? (
          // Confirm strip for a high-consequence action.
          <div className="flex items-center gap-[0.35rem] rounded-lg bg-background/70 px-[0.4rem] py-[0.3rem] backdrop-blur-sm">
            <span className="px-[0.2rem] text-[0.7rem] text-surface-foreground">
              {pending.label}?
            </span>
            <SkillButton
              icon={Check}
              label="Confirm"
              enabled={!busy}
              emphasis
              onClick={() => void run(pending)}
            />
            <SkillButton
              icon={X}
              label="Cancel"
              enabled={!busy}
              onClick={() => setPending(null)}
            />
          </div>
        ) : page === "mode" ? (
          // Mode-preset page for the FC's autopilot family.
          <div className="flex items-end gap-[0.35rem]">
            <SkillButton
              icon={ChevronLeft}
              label="Back"
              enabled
              onClick={() => setPage("primary")}
            />
            {modeSkills.map((skill) => {
              const state = resolveSkillState(skill, ctx);
              return (
                <SkillButton
                  key={skill.id}
                  icon={Gauge}
                  label={skill.label}
                  enabled={state.enabled && !busy}
                  reason={state.reason}
                  onClick={() => onSkill(skill, state)}
                />
              );
            })}
          </div>
        ) : (
          // Primary flight actions.
          <div className="flex items-end gap-[0.35rem]">
            {primarySkills.map((skill) => {
              const state = resolveSkillState(skill, ctx);
              return (
                <SkillButton
                  key={skill.id}
                  icon={CORE_ICONS[skill.id]}
                  label={skill.label}
                  enabled={state.enabled && !busy}
                  reason={state.reason}
                  emphasis
                  onClick={() => onSkill(skill, state)}
                />
              );
            })}
            <SkillButton
              icon={Sliders}
              label="Mode"
              enabled={modeState.enabled && !busy}
              reason={modeState.reason}
              onClick={() => {
                if (modeState.enabled && !busy) setPage("mode");
              }}
            />
          </div>
        )}
      </div>
    </div>
  );
}

/** A short, honest reason for a failed command send. A 503/400 carries a
 *  `{detail}` body the route sets; anything else falls back to a generic line. */
function errText(e: unknown): string {
  if (e instanceof ApiError) {
    const b = e.body;
    if (b && typeof b === "object" && "detail" in b) {
      return String((b as { detail: unknown }).detail);
    }
    return e.message;
  }
  return "command failed";
}
