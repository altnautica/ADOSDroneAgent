// The Launch sheet, opened by the left column's Take off (or a long-press on
// a map while on the ground, prefilled with that point): the take-off
// altitude, what to do once there (hold, a skill, the loaded mission, or a
// point), and an optional "when done" action. Holding Launch passes the
// pre-flight checklist gate (the first arm of a session) and sends the
// pilot's `launch` action; the sequence's progress (checks → arm → take off →
// after) shows as a step strip, and Abort brakes to a hold (or disarms while
// still on the ground). Ranges, defaults and texts come from the spec.

import { useEffect, useMemo, useState } from "react";

import { HoldButton } from "@/components/ui/hold-button";
import { Sheet } from "@/components/ui/sheet";
import { usePilot } from "@/hooks/use-pilot";
import { getParam } from "@/lib/api";
import { LAUNCH, LAUNCH_STEPS, launchArgs, launchRunning, launchStepText, type LaunchAfter, type WhenDone } from "@/lib/cockpit-flow";
import { BUILTIN_BY_ID } from "@/lib/cockpit-spec";
import { sendPilotAction } from "@/lib/pilot-link";
import { skillVisibleFor } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { abortLaunch, useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { requirePreflight } from "@/stores/preflight-store";

/** The launch sequence's step strip. */
function LaunchSteps() {
  const seq = usePilot()?.data.sequence ?? null;
  if (!seq) return null;
  const at = LAUNCH_STEPS.indexOf(seq.step);
  return (
    <div className="flex flex-col gap-[0.3rem]" data-testid="launch-steps">
      <ol className="flex gap-[0.25rem]">
        {LAUNCH_STEPS.map((s, i) => (
          <li
            key={s}
            aria-current={i === at ? "step" : undefined}
            className={cn(
              "flex-1 rounded border px-[0.3rem] py-[0.2rem] text-center text-[0.72rem] font-semibold",
              seq.error && i === at
                ? "border-err text-err"
                : i < at || seq.step === "done"
                  ? "border-ok/60 bg-ok/15 text-ok"
                  : i === at
                    ? "border-primary bg-primary/20"
                    : "border-border text-muted-foreground",
            )}
          >
            {launchStepText(s)}
          </li>
        ))}
      </ol>
      {seq.error ? <p className="text-[0.78rem] text-err">{refusalText(seq.error)}</p> : null}
    </div>
  );
}

function Stepper({ label, value, onChange, min, max, step = 1 }: {
  label: string;
  /** Null: not set yet; the first press starts from zero. */
  value: number | null;
  onChange: (v: number) => void;
  min: number;
  max: number;
  step?: number;
}) {
  const set = (v: number) => onChange(Math.min(max, Math.max(min, v)));
  return (
    <div className="flex items-center gap-[0.4rem]">
      <span className="min-w-[6rem] text-[0.82rem]">{label}</span>
      <button
        type="button"
        aria-label={specText("flow.stepper.down", { label })}
        onClick={() => set((value ?? 0) - step)}
        className="h-[48px] w-[48px] rounded-lg border border-border text-[1.1rem]"
      >
        −
      </button>
      <span className="min-w-[4.5rem] text-center font-mono text-[0.95rem]">
        {value === null ? "—" : specText("map.fly_here.distance_value", { dist: value })}
      </span>
      <button
        type="button"
        aria-label={specText("flow.stepper.up", { label })}
        onClick={() => set((value ?? 0) + step)}
        className="h-[48px] w-[48px] rounded-lg border border-border text-[1.1rem]"
      >
        +
      </button>
    </div>
  );
}

const WHEN_DONE: readonly WhenDone[] = ["none", "land", "rth"];

export function LaunchSheet() {
  const close = useCockpitUiStore((s) => s.closeSheet);
  const prefill = useCockpitUiStore((s) => s.launchGoto) ?? LAUNCH.gotoDefault;
  const pilot = usePilot()?.data ?? null;
  const extensions = useExtensionsStore((s) => s.extensions);
  const plane = pilot?.vehicleClass === "fixedwing";
  const [altM, setAltM] = useState(LAUNCH.altDefaultM);
  const [after, setAfter] = useState<LaunchAfter>(prefill ? "goto" : "hold");
  const [skill, setSkill] = useState<string | null>(null);
  const [gotoN, setGotoN] = useState<number | null>(prefill?.n ?? null);
  const [gotoE, setGotoE] = useState<number | null>(prefill?.e ?? null);
  const [whenDone, setWhenDone] = useState<WhenDone>("none");
  const result = useCockpitUiStore((s) => s.launchRefusal);

  // A plane takes off to its own TKOFF_ALT.
  useEffect(() => {
    if (!plane) return;
    const ac = new AbortController();
    void getParam("TKOFF_ALT", ac.signal).then((v) => {
      if (v !== null && v > 0) setAltM(Math.round(v));
    });
    return () => ac.abort();
  }, [plane]);

  const behaviors = useMemo(
    () =>
      extensions.flatMap((e) =>
        e.skills
          .filter((s) => s.category === "behavior" && s.extension?.toggle && skillVisibleFor(s, pilot?.vehicleClass))
          .map((s) => ({ id: s.id, label: s.label, plugin: e.name, pluginId: e.pluginId, skillId: s.id.slice(e.pluginId.length + 1) })),
      ),
    [extensions, pilot?.vehicleClass],
  );
  const missionLoaded = pilot?.mission?.state === "loaded";
  const sequenceRunning = launchRunning(pilot?.sequence ?? null);

  const options: { id: LaunchAfter; label: string; reason: string | null }[] = [
    { id: "hold", label: specText("flow.launch.after.hold"), reason: null },
    { id: "skill", label: specText("flow.launch.after.skill"), reason: behaviors.length === 0 ? specText("flow.launch.after.skill_none") : null },
    { id: "mission", label: specText("flow.launch.after.mission"), reason: missionLoaded ? null : specText("flow.launch.after.mission_none") },
    { id: "goto", label: specText("flow.launch.after.goto"), reason: null },
  ];

  const chosen = behaviors.find((b) => b.id === skill) ?? null;
  const goto = gotoN !== null && gotoE !== null ? { n: gotoN, e: gotoE } : null;
  const ready = pilot !== null && !(after === "skill" && !chosen) && !(after === "goto" && !goto);

  const launch = () =>
    requirePreflight(() => {
      const args = launchArgs({
        after,
        altM,
        skill: chosen ? { pluginId: chosen.pluginId, skillId: chosen.skillId } : null,
        goto: after === "goto" && goto ? { ...goto, altM } : null,
        whenDone,
      });
      const { setLaunchRefusal } = useCockpitUiStore.getState();
      setLaunchRefusal(null);
      void sendPilotAction("launch", args).then((r) =>
        setLaunchRefusal(r.ok ? null : specText("flow.launch.refused", { reason: refusalText(r.err) })),
      );
    });

  return (
    <Sheet
      title={specText("flow.launch.title")}
      onDismiss={close}
      footer={
        sequenceRunning ? (
          <button
            type="button"
            onClick={abortLaunch}
            className="min-h-[56px] w-full rounded-lg border border-err bg-err/20 text-[0.95rem] font-semibold text-err"
          >
            {specText("flow.launch.abort")}
          </button>
        ) : (
          <HoldButton
            holdMs={BUILTIN_BY_ID.takeoff.confirm.ms}
            disabled={!ready}
            onFired={launch}
            className="min-h-[56px] w-full text-[0.95rem]"
            label={specText("flow.launch.hold_aria")}
          >
            {specText("flow.launch.hold_to_launch")}
          </HoldButton>
        )
      }
    >
      {!pilot ? <p className="text-[0.85rem] text-err">{specText("flow.launch.no_pilot")}</p> : null}
      <LaunchSteps />
      {result ? <p role="status" className="text-[0.8rem] text-err">{result}</p> : null}
      <div className="flex flex-col gap-[0.6rem] py-[0.4rem]">
        {plane ? (
          <p className="text-[0.82rem] text-muted-foreground">{specText("flow.launch.plane_alt")}</p>
        ) : (
          <Stepper label={specText("flow.launch.alt_m")} value={altM} onChange={setAltM} min={LAUNCH.altMinM} max={LAUNCH.altMaxM} />
        )}
        <div className="flex flex-col gap-[0.3rem]">
          <span className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">{specText("flow.launch.after")}</span>
          <div className="grid grid-cols-2 gap-[0.3rem]" role="radiogroup">
            {options.map((o) => (
              <button
                key={o.id}
                type="button"
                role="radio"
                aria-checked={after === o.id}
                disabled={o.reason !== null}
                title={o.reason ?? undefined}
                onClick={() => setAfter(o.id)}
                className={cn(
                  "min-h-[48px] rounded-lg border text-[0.85rem] font-semibold disabled:opacity-45",
                  after === o.id ? "border-primary bg-primary/25" : "border-border",
                )}
              >
                {o.label}
                {o.reason ? <span className="block text-[0.7rem] font-normal text-muted-foreground">{o.reason}</span> : null}
              </button>
            ))}
          </div>
        </div>
        {after === "skill" ? (
          <div className="flex flex-col gap-[0.25rem]" role="radiogroup" aria-label={specText("flow.launch.after.skill_list")}>
            {behaviors.map((b) => (
              <button
                key={b.id}
                type="button"
                role="radio"
                aria-checked={skill === b.id}
                onClick={() => setSkill(b.id)}
                className={cn("min-h-[48px] rounded-lg border px-[0.6rem] text-left text-[0.85rem]", skill === b.id ? "border-primary bg-primary/25" : "border-border")}
              >
                {b.label} <span className="text-muted-foreground">· {b.plugin}</span>
              </button>
            ))}
          </div>
        ) : null}
        {after === "goto" ? (
          <div data-testid="launch-goto">
            <Stepper label={specText("flow.launch.goto.north")} value={gotoN} onChange={setGotoN} min={-500} max={500} step={5} />
            <Stepper label={specText("flow.launch.goto.east")} value={gotoE} onChange={setGotoE} min={-500} max={500} step={5} />
          </div>
        ) : null}
        <div className="flex flex-col gap-[0.3rem]">
          <span className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">{specText("flow.launch.when_done")}</span>
          <div className="flex gap-[0.3rem]" role="radiogroup">
            {WHEN_DONE.map((w) => (
              <button
                key={w}
                type="button"
                role="radio"
                aria-checked={whenDone === w}
                onClick={() => setWhenDone(w)}
                className={cn("min-h-[48px] flex-1 rounded-lg border text-[0.85rem]", whenDone === w ? "border-primary bg-primary/25" : "border-border")}
              >
                {specText(`flow.launch.when_done.${w}`)}
              </button>
            ))}
          </div>
        </div>
      </div>
    </Sheet>
  );
}
