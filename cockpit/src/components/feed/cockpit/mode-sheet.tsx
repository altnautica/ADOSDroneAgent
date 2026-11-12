// The Mode sheet, opened from the flight-state chip (cockpit spec
// `flight-state.json` `mode_sheet`): flight profile, control mode (ADOS Pilot /
// FC Manual / ELRS, hold to switch, ELRS only on a ground-station link),
// avoidance (Off needs a hold), handing the aircraft to the FC or taking
// control back, the VTOL transition, and every FC mode (advanced). Any change
// here stops a running motion skill alongside it. A pick shows at once: the
// picked control turns pending (outlined, busy) and only its own row waits
// for the answer or the timeout; the recovery holds and handing to the FC
// never wait. The result line then says what happened, in words. Current
// values come from a fresh pilot state only; a stale one reads "—".

import { useState, type ReactNode } from "react";
import { Loader2 } from "lucide-react";

import { HoldButton } from "@/components/ui/hold-button";
import { Sheet } from "@/components/ui/sheet";
import { useFreshPilot, usePilot } from "@/hooks/use-pilot";
import { avoidOffWarning, holdMs, modeHoldLabel, modeResultText } from "@/lib/cockpit-flow";
import { MODE_SHEET } from "@/lib/cockpit-spec";
import { fcFamily, fcModesFor } from "@/lib/fc-modes";
import { sendPilotAction } from "@/lib/pilot-link";
import { cn } from "@/lib/utils";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { useProfile } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useFlightStore } from "@/stores/flight-store";

import { modeChange, pickDisabled, sendFcMode, type Outcome, type PendingRows } from "./mode-sheet-pending";

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
  pending: PendingRows;
  /** Values that need a hold to select. */
  holdFor?: readonly string[];
  hold?: number;
}) {
  const busy = pending[row] !== undefined;
  return (
    <div className="flex gap-[0.3rem]" role="radiogroup" aria-busy={busy} data-row={row}>
      {values.map((v, i) => {
        const waiting = pending[row] === v;
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
  // The last pilot state lays the sheet out; current values and the Take
  // control gate read only a fresh one.
  const last = usePilot()?.data ?? null;
  const pilot = useFreshPilot();
  const autopilot = useFlightStore((s) => s.telemetry?.autopilot ?? null);
  const fcModeName = useFlightStore((s) => s.telemetry?.mode ?? null);
  const groundLink = useProfile() === "ground_station";
  const [result, setResult] = useState<{ text: string; ok: boolean } | null>(null);
  const [pending, setPending] = useState<PendingRows>({});
  const [advanced, setAdvanced] = useState(false);

  const family = fcFamily(autopilot, last?.vehicleClass ?? null);
  const cm = MODE_SHEET.control_mode;
  // ELRS is offered on a ground station, and kept wherever it is the current
  // mode (set from the ground station) so the row shows what is in force.
  const controlValues = cm.values.filter((v) => v !== "elrs" || groundLink || pilot?.controlMode === "elrs");

  /** Run one change: the control turns pending at once, its row waits for the
   *  answer (or the timeout), then the result line shows it. */
  const act = async (label: string, row: string, value: string, run: () => Promise<Outcome>) => {
    if (pickDisabled(pending, row, value)) return;
    setPending((p) => ({ ...p, [row]: value }));
    setResult(null);
    const r = await modeChange(run);
    setPending((p) => {
      if (p[row] !== value) return p;
      const next: Record<string, string> = { ...p };
      delete next[row];
      return next;
    });
    setResult({ text: modeResultText(label, r), ok: r.ok });
  };
  const pilotAction = (label: string, row: string, value: string, name: string, args?: Record<string, unknown>) =>
    void act(label, row, value, async () => {
      const r = await sendPilotAction(name, args);
      return r.ok ? { ok: true } : { ok: false, reason: refusalText(r.err) };
    });

  const handValues = family === "plane" ? MODE_SHEET.hand_to_fc.plane_values : MODE_SHEET.hand_to_fc.values;
  const handLabel = (v: string) => MODE_SHEET.hand_to_fc.labels[MODE_SHEET.hand_to_fc.values.indexOf(v)] ?? v;
  const waiting = (row: string, value: string) => pending[row] === value;
  const avoidWarning = avoidOffWarning(pilot?.avoid ?? null);

  return (
    <Sheet title={specText("flow.mode.title")} onDismiss={close}>
      {!last ? <p className="py-[0.5rem] text-[0.85rem] text-muted-foreground">{specText("flow.mode.no_pilot")}</p> : null}
      {last && !pilot ? (
        <p className="py-[0.5rem] text-[0.85rem] text-muted-foreground" data-testid="mode-stale">
          {specText("flow.mode.stale")}
        </p>
      ) : null}
      {result ? (
        <p role="status" data-testid="mode-result" className={cn("text-[0.8rem]", result.ok ? "text-ok" : "text-err")}>
          {result.text}
        </p>
      ) : null}

      {last ? (
        <>
          <Section title={specText("flow.mode.section.profile")}>
            <Segment
              row="profile"
              values={MODE_SHEET.profile.values}
              labels={MODE_SHEET.profile.labels}
              current={pilot?.profile ?? null}
              pending={pending}
              onPick={(v) => pilotAction(specText("flow.mode.section.profile"), "profile", v, `profile:${v}`)}
            />
          </Section>

          <Section title={specText("flow.mode.section.control_mode")}>
            <Segment
              row="control_mode"
              values={controlValues}
              labels={controlValues.map((v) => cm.labels[cm.values.indexOf(v)])}
              current={pilot?.controlMode ?? null}
              pending={pending}
              holdFor={controlValues}
              hold={cm.confirm.ms}
              onPick={(v) => pilotAction(specText("flow.mode.section.control_mode"), "control_mode", v, `control_mode:${v}`)}
            />
            <ul className="flex flex-col gap-[0.2rem] text-[0.78rem] text-muted-foreground">
              {controlValues.map((v) => (
                <li key={v} className={cn(pilot?.controlMode === v && "text-surface-foreground")}>
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
              current={pilot?.avoid ?? null}
              pending={pending}
              holdFor={["off"]}
              hold={MODE_SHEET.avoidance.confirm_off.ms}
              onPick={(v) => pilotAction(specText("flow.mode.section.avoidance"), "avoid", v, `avoid:${v}`)}
            />
            {avoidWarning ? (
              <p role="alert" className="text-[0.75rem] font-semibold text-warn" data-testid="avoid-off-warning">
                {avoidWarning}
              </p>
            ) : null}
            {last.vehicleClass === "fixedwing" ? (
              <p className="text-[0.75rem] text-muted-foreground" data-testid="fixedwing-note">
                {specText("pilot.avoid.fixedwing_note")}
              </p>
            ) : null}
          </Section>

          <Section title={specText("flow.mode.section.who_flies")}>
            {last.owner === "fc_mode" || last.owner === "idle" ? (
              <HoldButton
                holdMs={holdMs("take_control")}
                disabled={!pilot?.armed || pickDisabled(pending, "who", "take")}
                onFired={() => pilotAction(specText("flow.take_control"), "who", "take", "take_control")}
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
                    className={cn("min-h-[48px] flex-1", waiting("who", v) && PENDING_CLASS)}
                    onFired={() => pilotAction(specText("flow.hand_to_fc"), "who", v, "hand_to_fc", { mode: v })}
                    label={specText("flow.mode.hand_to_fc_aria", { mode: handLabel(v) })}
                  >
                    {specText("flow.mode.hand_to_fc_button", { mode: handLabel(v) })}
                    {waiting("who", v) ? <Busy /> : null}
                  </HoldButton>
                ))}
              </div>
            )}
          </Section>

          {last.vehicleClass === "vtol" ? (
            <Section title={specText("flow.mode.section.vtol")}>
              <div className="flex gap-[0.3rem]">
                {(["fw", "mc"] as const).map((to) => (
                  <HoldButton
                    key={to}
                    holdMs={holdMs("vtol_transition")}
                    disabled={pickDisabled(pending, "vtol", to)}
                    className={cn("min-h-[48px] flex-1", waiting("vtol", to) && PENDING_CLASS)}
                    onFired={() => pilotAction(specText("flow.mode.section.vtol"), "vtol", to, `vtol:${to}`)}
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
                  disabled={pickDisabled(pending, "fc", m.name)}
                  className={cn("min-h-[48px]", waiting("fc", m.name) && PENDING_CLASS)}
                  label={specText("flow.mode.fc.hold_aria", { mode: m.label })}
                  onFired={() => void act(m.label, "fc", m.name, () => sendFcMode(m.name))}
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
