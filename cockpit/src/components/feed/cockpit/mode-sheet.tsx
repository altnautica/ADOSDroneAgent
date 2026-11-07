// The Mode sheet, opened from the flight-state chip (cockpit spec
// `flight-state.json` `mode_sheet`): flight profile, control mode (ADOS Pilot /
// FC Manual / ELRS, hold to switch, ELRS only on a ground-station link),
// avoidance (Off needs a hold), handing the aircraft to the FC or taking
// control back, the VTOL transition, and every FC mode (advanced). Any change
// here stops a running motion skill first.

import { useState, type ReactNode } from "react";

import { HoldButton } from "@/components/ui/hold-button";
import { Sheet } from "@/components/ui/sheet";
import { usePilot } from "@/hooks/use-pilot";
import { sendCommand } from "@/lib/api";
import { MODE_SHEET } from "@/lib/cockpit-spec";
import { fcFamily, fcModesFor } from "@/lib/fc-modes";
import { sendPilotAction } from "@/lib/pilot-link";
import { stopMotionSkills } from "@/lib/skill-runner";
import { cn } from "@/lib/utils";
import { useProfile } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useFlightStore } from "@/stores/flight-store";

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-[0.4rem] py-[0.5rem]">
      <h3 className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      {children}
    </section>
  );
}

function Segment({
  values,
  labels,
  current,
  onPick,
  holdFor,
  holdMs,
}: {
  values: readonly string[];
  labels: readonly string[];
  current: string | null;
  onPick: (v: string) => void;
  /** Values that need a hold to select. */
  holdFor?: readonly string[];
  holdMs?: number;
}) {
  return (
    <div className="flex gap-[0.3rem]" role="radiogroup">
      {values.map((v, i) =>
        holdFor?.includes(v) && v !== current ? (
          <HoldButton key={v} holdMs={holdMs} onFired={() => onPick(v)} className="min-h-[48px] flex-1" label={`${labels[i]} (hold)`}>
            {labels[i]}
          </HoldButton>
        ) : (
          <button
            key={v}
            type="button"
            role="radio"
            aria-checked={current === v}
            onClick={() => v !== current && onPick(v)}
            className={cn(
              "min-h-[48px] flex-1 rounded-lg border px-[0.6rem] text-[0.85rem] font-semibold",
              current === v ? "border-primary bg-primary/25" : "border-border bg-surface hover:bg-muted",
            )}
          >
            {labels[i]}
          </button>
        ),
      )}
    </div>
  );
}

export function ModeSheet() {
  const close = useCockpitUiStore((s) => s.closeSheet);
  const pilot = usePilot()?.data ?? null;
  const autopilot = useFlightStore((s) => s.telemetry?.autopilot ?? null);
  const fcModeName = useFlightStore((s) => s.telemetry?.mode ?? null);
  const groundLink = useProfile() === "ground_station";
  const [result, setResult] = useState<{ text: string; ok: boolean } | null>(null);
  const [advanced, setAdvanced] = useState(false);

  const family = fcFamily(autopilot, pilot?.vehicleClass ?? null);
  const cm = MODE_SHEET.control_mode;
  const controlValues = cm.values.filter((v) => v !== "elrs" || groundLink);

  const act = async (label: string, run: () => Promise<{ ok: boolean; err?: string }>) => {
    await stopMotionSkills();
    const r = await run();
    setResult({ text: r.ok ? `${label}: done` : `${label}: ${r.err ?? "refused"}`, ok: r.ok });
  };
  const pilotAction = (label: string, name: string, args?: Record<string, unknown>) =>
    void act(label, () => sendPilotAction(name, args));

  const handValues = family === "plane" ? MODE_SHEET.hand_to_fc.plane_values : MODE_SHEET.hand_to_fc.values;
  const handLabel = (v: string) => MODE_SHEET.hand_to_fc.labels[MODE_SHEET.hand_to_fc.values.indexOf(v)] ?? v;

  return (
    <Sheet title="Flight mode" onDismiss={close}>
      {!pilot ? (
        <p className="py-[0.5rem] text-[0.85rem] text-muted-foreground">
          ADOS Pilot is not reporting on this node. FC modes can still be set below.
        </p>
      ) : null}
      {result ? (
        <p role="status" className={cn("text-[0.8rem]", result.ok ? "text-ok" : "text-err")}>
          {result.text}
        </p>
      ) : null}

      {pilot ? (
        <>
          <Section title="Profile">
            <Segment
              values={MODE_SHEET.profile.values}
              labels={MODE_SHEET.profile.labels}
              current={pilot.profile}
              onPick={(v) => pilotAction("Profile", `profile:${v}`)}
            />
          </Section>

          <Section title="Control mode">
            <Segment
              values={controlValues}
              labels={controlValues.map((v) => cm.labels[cm.values.indexOf(v)])}
              current={pilot.controlMode}
              holdFor={controlValues}
              holdMs={cm.confirm.ms}
              onPick={(v) => pilotAction("Control mode", `control_mode:${v}`)}
            />
            <ul className="flex flex-col gap-[0.2rem] text-[0.78rem] text-muted-foreground">
              {controlValues.map((v) => (
                <li key={v} className={cn(pilot.controlMode === v && "text-surface-foreground")}>
                  <span className="font-semibold">{cm.labels[cm.values.indexOf(v)]}:</span>{" "}
                  {cm.explain[v as keyof typeof cm.explain]}
                </li>
              ))}
            </ul>
          </Section>

          <Section title="Avoidance">
            <Segment
              values={MODE_SHEET.avoidance.values}
              labels={MODE_SHEET.avoidance.labels}
              current={pilot.avoid}
              holdFor={["off"]}
              holdMs={MODE_SHEET.avoidance.confirm_off.ms}
              onPick={(v) => pilotAction("Avoidance", `avoid:${v}`)}
            />
            {pilot.vehicleClass === "fixedwing" ? (
              <p className="text-[0.75rem] text-muted-foreground">
                Fixed-wing: avoidance keeps the minimum height above ground only; the planar LiDAR is not used.
              </p>
            ) : null}
          </Section>

          <Section title="Who flies">
            {pilot.owner === "fc_mode" || pilot.owner === "idle" ? (
              <button
                type="button"
                onClick={() => pilotAction("Take control", "take_control")}
                disabled={!pilot.armed}
                className="min-h-[48px] rounded-lg border border-primary bg-primary/25 text-[0.85rem] font-semibold disabled:opacity-45"
              >
                Take control (ADOS Pilot)
              </button>
            ) : (
              <div className="flex gap-[0.3rem]">
                {handValues.map((v) => (
                  <HoldButton
                    key={v}
                    holdMs={1500}
                    className="min-h-[48px] flex-1"
                    onFired={() => pilotAction("Hand to FC", "hand_to_fc", { mode: v })}
                    label={`Hand to FC in ${handLabel(v)} (hold)`}
                  >
                    FC · {handLabel(v)}
                  </HoldButton>
                ))}
              </div>
            )}
          </Section>

          {pilot.vehicleClass === "vtol" ? (
            <Section title="VTOL">
              <div className="flex gap-[0.3rem]">
                <HoldButton holdMs={1500} className="min-h-[48px] flex-1" onFired={() => pilotAction("Transition", "vtol:fw")}>
                  To forward flight
                </HoldButton>
                <HoldButton holdMs={1500} className="min-h-[48px] flex-1" onFired={() => pilotAction("Transition", "vtol:mc")}>
                  To hover
                </HoldButton>
              </div>
            </Section>
          ) : null}
        </>
      ) : null}

      <Section title="FC modes (advanced)">
        {!advanced ? (
          <button
            type="button"
            onClick={() => setAdvanced(true)}
            className="min-h-[48px] rounded-lg border border-border text-[0.85rem]"
          >
            Show every FC mode
          </button>
        ) : (
          <div className="grid grid-cols-2 gap-[0.3rem]">
            {fcModesFor(family).map((m) => {
              const current = fcModeName?.toUpperCase() === m.name;
              return current ? (
                <span
                  key={m.name}
                  aria-current="true"
                  className="flex min-h-[48px] items-center justify-center rounded-lg border border-primary bg-primary/25 text-[0.82rem] font-semibold"
                >
                  {m.label} · current
                </span>
              ) : (
                <HoldButton
                  key={m.name}
                  holdMs={m.holdMs}
                  className="min-h-[48px]"
                  label={`${m.label} (hold)`}
                  onFired={() =>
                    void act(m.label, async () => {
                      try {
                        const r = await sendCommand("mode", [m.name]);
                        return r.ack?.observed && !r.ack.accepted
                          ? { ok: false, err: r.ack.result_name ?? "rejected" }
                          : { ok: true };
                      } catch {
                        return { ok: false, err: "command failed" };
                      }
                    })
                  }
                >
                  {m.label}
                </HoldButton>
              );
            })}
          </div>
        )}
      </Section>
    </Sheet>
  );
}
