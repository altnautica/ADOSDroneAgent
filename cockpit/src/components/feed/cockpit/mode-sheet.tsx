// The Mode sheet, opened from the flight-state chip (cockpit spec
// `flight-state.json` `mode_sheet`): flight profile, control mode (ADOS Pilot /
// FC Manual / ELRS, hold to switch, ELRS only on a ground-station link),
// avoidance (Off needs a hold), handing the aircraft to the FC or taking
// control back, the VTOL transition, and every FC mode (advanced). Any change
// here stops a running motion skill first. A pick shows at once: the picked
// control turns pending (outlined, busy) and its row waits for the answer or
// the timeout, then the result line says what happened, in words.

import { useState, type ReactNode } from "react";
import { Loader2 } from "lucide-react";

import { HoldButton } from "@/components/ui/hold-button";
import { Sheet } from "@/components/ui/sheet";
import { usePilot } from "@/hooks/use-pilot";
import { sendCommand } from "@/lib/api";
import { avoidOffWarning, holdMs, mavResultText, modeHoldLabel, modeResultText } from "@/lib/cockpit-flow";
import { MODE_SHEET } from "@/lib/cockpit-spec";
import { fcFamily, fcModesFor } from "@/lib/fc-modes";
import { sendPilotAction } from "@/lib/pilot-link";
import { stopMotionSkills } from "@/lib/skill-runner";
import { cn } from "@/lib/utils";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { useProfile } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useFlightStore } from "@/stores/flight-store";

/** The control waiting for an answer: its row and value. */
interface Pending {
  row: string;
  value: string;
}

type Outcome = { ok: boolean; reason?: string };

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-[0.4rem] py-[0.5rem]">
      <h3 className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      {children}
    </section>
  );
}

/** The busy mark inside a pending control. */
function Busy() {
  return <Loader2 className="ml-[0.3rem] inline h-[1em] w-[1em] animate-spin motion-reduce:animate-none" aria-hidden />;
}

const PENDING_CLASS = "border-2 border-dashed border-primary";

function Segment({
  row,
  values,
  labels,
  current,
  onPick,
  pending,
  holdFor,
  hold,
}: {
  row: string;
  values: readonly string[];
  labels: readonly string[];
  current: string | null;
  onPick: (v: string) => void;
  pending: Pending | null;
  /** Values that need a hold to select. */
  holdFor?: readonly string[];
  hold?: number;
}) {
  const busy = pending !== null;
  return (
    <div className="flex gap-[0.3rem]" role="radiogroup" aria-busy={pending?.row === row} data-row={row}>
      {values.map((v, i) => {
        const waiting = pending?.row === row && pending.value === v;
        return holdFor?.includes(v) && v !== current ? (
          <HoldButton
            key={v}
            holdMs={hold}
            disabled={busy}
            onFired={() => onPick(v)}
            className={cn("min-h-[48px] flex-1", waiting && PENDING_CLASS)}
            label={modeHoldLabel(row, v, labels[i])}
          >
            {labels[i]}
            {waiting ? <Busy /> : null}
          </HoldButton>
        ) : (
          <button
            key={v}
            type="button"
            role="radio"
            aria-checked={current === v}
            aria-busy={waiting}
            disabled={busy}
            data-pending={waiting || undefined}
            onClick={() => v !== current && onPick(v)}
            className={cn(
              "min-h-[48px] flex-1 rounded-lg border px-[0.6rem] text-[0.85rem] font-semibold disabled:cursor-wait",
              current === v ? "border-primary bg-primary/25" : "border-border bg-surface hover:bg-muted",
              waiting && PENDING_CLASS,
              busy && !waiting && "opacity-60",
            )}
          >
            {labels[i]}
            {waiting ? <Busy /> : null}
          </button>
        );
      })}
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
  const [pending, setPending] = useState<Pending | null>(null);
  const [advanced, setAdvanced] = useState(false);

  const family = fcFamily(autopilot, pilot?.vehicleClass ?? null);
  const cm = MODE_SHEET.control_mode;
  const controlValues = cm.values.filter((v) => v !== "elrs" || groundLink);

  /** Run one change: the control turns pending at once, every control waits
   *  for the answer (or the timeout), then the result line shows it. */
  const act = async (label: string, at: Pending, run: () => Promise<Outcome>) => {
    if (pending) return;
    setPending(at);
    setResult(null);
    await stopMotionSkills();
    const r = await run();
    setPending(null);
    setResult({ text: modeResultText(label, r), ok: r.ok });
  };
  const pilotAction = (label: string, at: Pending, name: string, args?: Record<string, unknown>) =>
    void act(label, at, async () => {
      const r = await sendPilotAction(name, args);
      return r.ok ? { ok: true } : { ok: false, reason: refusalText(r.err) };
    });

  const handValues = family === "plane" ? MODE_SHEET.hand_to_fc.plane_values : MODE_SHEET.hand_to_fc.values;
  const handLabel = (v: string) => MODE_SHEET.hand_to_fc.labels[MODE_SHEET.hand_to_fc.values.indexOf(v)] ?? v;
  const busy = pending !== null;
  const waiting = (row: string, value: string) => pending?.row === row && pending.value === value;

  return (
    <Sheet title={specText("flow.mode.title")} onDismiss={close}>
      {!pilot ? <p className="py-[0.5rem] text-[0.85rem] text-muted-foreground">{specText("flow.mode.no_pilot")}</p> : null}
      {result ? (
        <p role="status" data-testid="mode-result" className={cn("text-[0.8rem]", result.ok ? "text-ok" : "text-err")}>
          {result.text}
        </p>
      ) : null}

      {pilot ? (
        <>
          <Section title={specText("flow.mode.section.profile")}>
            <Segment
              row="profile"
              values={MODE_SHEET.profile.values}
              labels={MODE_SHEET.profile.labels}
              current={pilot.profile}
              pending={pending}
              onPick={(v) => pilotAction(specText("flow.mode.section.profile"), { row: "profile", value: v }, `profile:${v}`)}
            />
          </Section>

          <Section title={specText("flow.mode.section.control_mode")}>
            <Segment
              row="control_mode"
              values={controlValues}
              labels={controlValues.map((v) => cm.labels[cm.values.indexOf(v)])}
              current={pilot.controlMode}
              pending={pending}
              holdFor={controlValues}
              hold={cm.confirm.ms}
              onPick={(v) => pilotAction(specText("flow.mode.section.control_mode"), { row: "control_mode", value: v }, `control_mode:${v}`)}
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

          <Section title={specText("flow.mode.section.avoidance")}>
            <Segment
              row="avoid"
              values={MODE_SHEET.avoidance.values}
              labels={MODE_SHEET.avoidance.labels}
              current={pilot.avoid}
              pending={pending}
              holdFor={["off"]}
              hold={MODE_SHEET.avoidance.confirm_off.ms}
              onPick={(v) => pilotAction(specText("flow.mode.section.avoidance"), { row: "avoid", value: v }, `avoid:${v}`)}
            />
            {avoidOffWarning(pilot.avoid) ? (
              <p role="alert" className="text-[0.75rem] font-semibold text-warn" data-testid="avoid-off-warning">
                {avoidOffWarning(pilot.avoid)}
              </p>
            ) : null}
            {pilot.vehicleClass === "fixedwing" ? (
              <p className="text-[0.75rem] text-muted-foreground" data-testid="fixedwing-note">
                {specText("pilot.avoid.fixedwing_note")}
              </p>
            ) : null}
          </Section>

          <Section title={specText("flow.mode.section.who_flies")}>
            {pilot.owner === "fc_mode" || pilot.owner === "idle" ? (
              <HoldButton
                holdMs={holdMs("take_control")}
                disabled={!pilot.armed || busy}
                onFired={() => pilotAction(specText("flow.take_control"), { row: "who", value: "take" }, "take_control")}
                className={cn("min-h-[48px] border-primary bg-primary/25", waiting("who", "take") && PENDING_CLASS)}
              >
                {specText("flow.mode.take_control")}
                {waiting("who", "take") ? <Busy /> : null}
              </HoldButton>
            ) : (
              <div className="flex gap-[0.3rem]">
                {handValues.map((v) => (
                  <HoldButton
                    key={v}
                    holdMs={holdMs("hand_to_fc")}
                    disabled={busy}
                    className={cn("min-h-[48px] flex-1", waiting("who", v) && PENDING_CLASS)}
                    onFired={() => pilotAction(specText("flow.hand_to_fc"), { row: "who", value: v }, "hand_to_fc", { mode: v })}
                    label={specText("flow.mode.hand_to_fc_aria", { mode: handLabel(v) })}
                  >
                    {specText("flow.mode.hand_to_fc_button", { mode: handLabel(v) })}
                    {waiting("who", v) ? <Busy /> : null}
                  </HoldButton>
                ))}
              </div>
            )}
          </Section>

          {pilot.vehicleClass === "vtol" ? (
            <Section title={specText("flow.mode.section.vtol")}>
              <div className="flex gap-[0.3rem]">
                {(["fw", "mc"] as const).map((to) => (
                  <HoldButton
                    key={to}
                    holdMs={holdMs("vtol_transition")}
                    disabled={busy}
                    className={cn("min-h-[48px] flex-1", waiting("vtol", to) && PENDING_CLASS)}
                    onFired={() => pilotAction(specText("flow.mode.section.vtol"), { row: "vtol", value: to }, `vtol:${to}`)}
                  >
                    {specText(to === "fw" ? "flow.mode.vtol.to_fw" : "flow.mode.vtol.to_mc")}
                    {waiting("vtol", to) ? <Busy /> : null}
                  </HoldButton>
                ))}
              </div>
            </Section>
          ) : null}
        </>
      ) : null}

      <Section title={specText("flow.mode.section.fc_modes")}>
        {!advanced ? (
          <>
            <p className="text-[0.75rem] text-muted-foreground" data-testid="fc-hidden-note">
              {specText("flow.mode.fc.hidden_note")}
            </p>
            <button
              type="button"
              onClick={() => setAdvanced(true)}
              className="min-h-[48px] rounded-lg border border-border text-[0.85rem]"
            >
              {specText("flow.mode.fc.show_all")}
            </button>
          </>
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
                  {specText("flow.mode.fc.current", { mode: m.label })}
                </span>
              ) : (
                <HoldButton
                  key={m.name}
                  holdMs={m.holdMs}
                  disabled={busy}
                  className={cn("min-h-[48px]", waiting("fc", m.name) && PENDING_CLASS)}
                  label={specText("flow.mode.fc.hold_aria", { mode: m.label })}
                  onFired={() =>
                    void act(m.label, { row: "fc", value: m.name }, async () => {
                      try {
                        const r = await sendCommand("mode", [m.name]);
                        if (!r.ack?.observed || r.ack.accepted) return { ok: true };
                        return { ok: false, reason: r.ack.result_name ? mavResultText(r.ack.result_name) : specText("flow.mode.fc.rejected") };
                      } catch {
                        return { ok: false, reason: specText("flow.mode.fc.failed") };
                      }
                    })
                  }
                >
                  {m.label}
                  {waiting("fc", m.name) ? <Busy /> : null}
                </HoldButton>
              );
            })}
          </div>
        )}
      </Section>
    </Sheet>
  );
}
