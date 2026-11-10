// The Launch sheet, opened by the left column's Take off: the take-off
// altitude, what to do once there (hold, a skill, the loaded mission, or a
// point), and an optional "when done" action. Holding Launch (1.5 s) passes the
// pre-flight checklist gate and sends the pilot's `launch` action; the
// sequence's progress (checks → arm → take off → after) shows as a step strip,
// and Abort brakes to a hold (or disarms while still on the ground).

import { useEffect, useMemo, useState } from "react";

import { HoldButton } from "@/components/ui/hold-button";
import { Sheet } from "@/components/ui/sheet";
import { usePilot } from "@/hooks/use-pilot";
import { getParam } from "@/lib/api";
import { ALT_MAX_M, ALT_MIN_M, COPTER_TAKEOFF_ALT_M, launchArgs, type LaunchAfter, type WhenDone } from "@/lib/cockpit-flow";
import { BUILTIN_BY_ID } from "@/lib/cockpit-spec";
import { sendPilotAction } from "@/lib/pilot-link";
import { pilotRefusal } from "@/lib/pilot-refusal";
import { skillVisibleFor } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { requirePreflight } from "@/stores/preflight-store";

const STEPS = [
  { id: "checks", label: "Checks" },
  { id: "arm", label: "Arm" },
  { id: "takeoff", label: "Take off" },
  { id: "after", label: "After" },
  { id: "done", label: "Done" },
] as const;

/** The launch sequence's step strip. Exported: the chip collapses into it. */
export function LaunchSteps() {
  const seq = usePilot()?.data.sequence ?? null;
  if (!seq) return null;
  const at = STEPS.findIndex((s) => s.id === seq.step);
  return (
    <div className="flex flex-col gap-[0.3rem]" data-testid="launch-steps">
      <ol className="flex gap-[0.25rem]">
        {STEPS.map((s, i) => (
          <li
            key={s.id}
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
            {s.label}
          </li>
        ))}
      </ol>
      {seq.error ? <p className="text-[0.78rem] text-err">{seq.error}</p> : null}
    </div>
  );
}

function Stepper({ label, value, onChange, min, max, step = 1, unit }: {
  label: string;
  value: number;
  onChange: (v: number) => void;
  min: number;
  max: number;
  step?: number;
  unit: string;
}) {
  const set = (v: number) => onChange(Math.min(max, Math.max(min, v)));
  return (
    <div className="flex items-center gap-[0.4rem]">
      <span className="min-w-[6rem] text-[0.82rem]">{label}</span>
      <button type="button" aria-label={`${label} down`} onClick={() => set(value - step)} className="h-[48px] w-[48px] rounded-lg border border-border text-[1.1rem]">
        −
      </button>
      <span className="min-w-[4.5rem] text-center font-mono text-[0.95rem]">
        {value} {unit}
      </span>
      <button type="button" aria-label={`${label} up`} onClick={() => set(value + step)} className="h-[48px] w-[48px] rounded-lg border border-border text-[1.1rem]">
        +
      </button>
    </div>
  );
}

export function LaunchSheet() {
  const close = useCockpitUiStore((s) => s.closeSheet);
  const pilot = usePilot()?.data ?? null;
  const extensions = useExtensionsStore((s) => s.extensions);
  const plane = pilot?.vehicleClass === "fixedwing";
  const [altM, setAltM] = useState(COPTER_TAKEOFF_ALT_M);
  const [after, setAfter] = useState<LaunchAfter>("hold");
  const [skill, setSkill] = useState<string | null>(null);
  const [gotoN, setGotoN] = useState(20);
  const [gotoE, setGotoE] = useState(0);
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
  const sequenceRunning = pilot?.sequence != null && pilot.sequence.step !== "done" && !pilot.sequence.error;

  const options: { id: LaunchAfter; label: string; reason: string | null }[] = [
    { id: "hold", label: "Hold", reason: null },
    { id: "skill", label: "Skill", reason: behaviors.length === 0 ? "No behaviour skill installed" : null },
    { id: "mission", label: "Start mission", reason: missionLoaded ? null : "No mission loaded on the drone" },
    { id: "goto", label: "Go to a point", reason: null },
  ];

  const chosen = behaviors.find((b) => b.id === skill) ?? null;
  const ready = !(after === "skill" && !chosen) && pilot !== null;

  const launch = () =>
    requirePreflight(() => {
      const args = launchArgs({
        after,
        altM,
        skill: chosen ? { pluginId: chosen.pluginId, skillId: chosen.skillId } : null,
        goto: after === "goto" ? { n: gotoN, e: gotoE, altM } : null,
        whenDone,
      });
      const { setLaunchRefusal } = useCockpitUiStore.getState();
      setLaunchRefusal(null);
      void sendPilotAction("launch", args).then((r) => setLaunchRefusal(r.ok ? null : `Launch refused: ${pilotRefusal(r.err)}`));
    });

  return (
    <Sheet
      title="Launch"
      onDismiss={close}
      footer={
        sequenceRunning ? (
          <button
            type="button"
            onClick={() =>
              void sendPilotAction("pause").then((r) =>
                useCockpitUiStore.getState().setLaunchRefusal(r.ok ? null : `Abort refused: ${pilotRefusal(r.err)}`),
              )
            }
            className="min-h-[56px] w-full rounded-lg border border-err bg-err/20 text-[0.95rem] font-semibold text-err"
          >
            Abort
          </button>
        ) : (
          <HoldButton
            holdMs={BUILTIN_BY_ID.takeoff.confirm.ms}
            disabled={!ready}
            onFired={launch}
            className="min-h-[56px] w-full text-[0.95rem]"
            label="Launch (hold)"
          >
            Hold to launch
          </HoldButton>
        )
      }
    >
      {!pilot ? <p className="text-[0.85rem] text-err">ADOS Pilot is not reporting: launch is unavailable.</p> : null}
      <LaunchSteps />
      {result ? <p role="status" className="text-[0.8rem] text-err">{result}</p> : null}
      <div className="flex flex-col gap-[0.6rem] py-[0.4rem]">
        <Stepper label="Take-off altitude" value={altM} onChange={setAltM} min={ALT_MIN_M} max={ALT_MAX_M} unit="m" />
        <div className="flex flex-col gap-[0.3rem]">
          <span className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">After take-off</span>
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
          <div className="flex flex-col gap-[0.25rem]" role="radiogroup" aria-label="Skill after take-off">
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
          <>
            <Stepper label="North of home" value={gotoN} onChange={setGotoN} min={-500} max={500} step={5} unit="m" />
            <Stepper label="East of home" value={gotoE} onChange={setGotoE} min={-500} max={500} step={5} unit="m" />
          </>
        ) : null}
        <div className="flex flex-col gap-[0.3rem]">
          <span className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">When done</span>
          <div className="flex gap-[0.3rem]" role="radiogroup">
            {(["none", "land", "rth"] as const).map((w) => (
              <button
                key={w}
                type="button"
                role="radio"
                aria-checked={whenDone === w}
                onClick={() => setWhenDone(w)}
                className={cn("min-h-[48px] flex-1 rounded-lg border text-[0.85rem]", whenDone === w ? "border-primary bg-primary/25" : "border-border")}
              >
                {w === "none" ? "Nothing" : w === "land" ? "Land" : "Return home"}
              </button>
            ))}
          </div>
        </div>
      </div>
    </Sheet>
  );
}
