// The confirm sheet for every non-tap skill, on the shared sheet. One sheet,
// several input paths: press-and-hold (touch/pointer), a held controller
// button or Enter, and a long-press of the panel's activate button (a hold no
// longer than the long press; the STOP takes a second panel long-press, see
// `dispatchPanelEvent`). The hold time is the spec's (`builtins.json` `confirm.ms`). The emergency stop (a
// `hold_confirm` skill) is the canonical STOP sheet: title `cockpit.estop`,
// body `confirm.kill.message`, one hold button labelled `stop.hold_label`
// that counts down (`hold.keep_holding`) while held, and fires once the hold
// completes. No second confirm step.

import { useEffect, useRef, useState } from "react";
import { ShieldAlert } from "lucide-react";

import { NumberSettingRow } from "@/components/ui/number-setting-row";
import { Sheet } from "@/components/ui/sheet";
import { panelLongPressCompletes } from "@/hooks/use-buttons";
import { useSkillContext } from "@/hooks/use-skill-context";
import { LAUNCH } from "@/lib/cockpit-flow";
import { executeSkill } from "@/lib/skill-runner";
import { resolveSkillState } from "@/lib/skills";
import { stopHoldText, stopSheetBody, stopSheetTitle } from "@/lib/stop-sheet";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useConfirmStore } from "@/stores/confirm-store";

export function SkillConfirmSheet() {
  const pending = useConfirmStore((s) => s.pending);
  if (!pending) return null;
  return <ConfirmSheet key={pending.skill.id} />;
}

function ConfirmSheet() {
  const pending = useConfirmStore((s) => s.pending)!;
  const heldSince = useConfirmStore((s) => s.heldSince);
  const panelNonce = useConfirmStore((s) => s.panelConfirmNonce);
  const cancel = useConfirmStore((s) => s.cancel);
  const setAltitude = useConfirmStore((s) => s.setAltitude);
  const setHeld = useConfirmStore((s) => s.setHeld);
  const ctx = useSkillContext();

  const { skill, altitudeM } = pending;
  const holdMs = skill.confirm.ms;
  const state = resolveSkillState(skill, ctx);
  const [pointerSince, setPointerSince] = useState<number | null>(null);
  const [progress, setProgress] = useState(0);
  const firedRef = useRef(false);
  const initialNonce = useRef(panelNonce);
  const confirmRef = useRef<HTMLDivElement>(null);

  const execute = () => {
    const current = useConfirmStore.getState().pending;
    cancel();
    if (current) void executeSkill(current.skill, { altitudeM: current.altitudeM, active: state.active });
  };
  const fire = () => {
    if (firedRef.current) return;
    firedRef.current = true;
    execute();
  };

  // The skill stopped being drivable while the sheet was up: close it.
  useEffect(() => {
    if (!state.enabled) cancel();
  }, [state.enabled, cancel]);


  // A panel long-press is a completed hold in one gesture, for a hold no
  // longer than the long press (the STOP is fired by the panel dispatch).
  useEffect(() => {
    if (panelNonce !== initialNonce.current && panelLongPressCompletes(holdMs)) fire();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [panelNonce]);

  // Enter held confirms; Escape cancels. Enter on another control in the
  // sheet (the altitude stepper) acts on that control, never as a hold.
  useEffect(() => {
    const down = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        cancel();
        return;
      }
      if (e.key !== "Enter" || e.repeat) return;
      const t = e.target;
      if (t instanceof HTMLElement && t !== document.body && !confirmRef.current?.contains(t)) return;
      setHeld(true);
    };
    const up = (e: KeyboardEvent) => {
      if (e.key === "Enter") setHeld(false);
    };
    window.addEventListener("keydown", down);
    window.addEventListener("keyup", up);
    return () => {
      window.removeEventListener("keydown", down);
      window.removeEventListener("keyup", up);
    };
  }, [cancel, setHeld]);

  // Hold progress, animated only while something is held.
  const since = pointerSince ?? heldSince;
  useEffect(() => {
    if (since === null) return;
    let raf = 0;
    const tick = () => {
      const p = Math.min(1, (performance.now() - since) / holdMs);
      setProgress(p);
      if (p >= 1) fire();
      else raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [since, holdMs]);

  const holding = since !== null;
  const shownProgress = holding ? progress : 0;
  const danger = skill.confirm.kind === "hold_confirm";

  const confirmControl = (
    <button
      type="button"
      data-testid={danger ? "stop-sheet-hold" : undefined}
      aria-label={danger ? specText("stop.hold_label") : undefined}
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture?.(e.pointerId);
        setPointerSince(performance.now());
      }}
      onPointerUp={() => setPointerSince(null)}
      onPointerCancel={() => setPointerSince(null)}
      onPointerLeave={() => setPointerSince(null)}
      className={cn(
        "relative flex min-h-[3.4rem] w-full touch-none select-none items-center justify-center overflow-hidden rounded-lg font-semibold",
        danger ? "bg-err/25 text-err" : "bg-primary/20 text-surface-foreground",
      )}
    >
      <span
        className={cn("absolute inset-y-0 left-0", danger ? "bg-err/45" : "bg-primary/45")}
        style={{ width: `${shownProgress * 100}%` }}
        aria-hidden
      />
      <span className="relative text-[0.9rem]">
        {danger ? stopHoldText(holding, holdMs, progress) : `Hold to confirm (${(holdMs / 1000).toFixed(1)} s)`}
      </span>
    </button>
  );

  return (
    <Sheet
      title={
        <span className="flex items-center gap-[0.4rem]" data-testid={danger ? "stop-sheet-title" : undefined}>
          {danger ? <ShieldAlert className="h-[1.2rem] w-[1.2rem] text-err" aria-hidden /> : null}
          {danger ? stopSheetTitle() : skill.label}
        </span>
      }
      onDismiss={cancel}
      danger={danger}
      initialFocus={confirmRef}
      closeLabel={specText("setup.action.cancel")}
      hints={[
        { command: "activate", label: "Hold to confirm" },
        { command: "back", label: "Cancel" },
      ]}
      footer={
        <>
          <div ref={confirmRef}>{confirmControl}</div>
          <p className="mt-[0.4rem] text-center text-[0.75rem] text-muted-foreground">
            {danger
              ? "Gamepad: hold the button · Keyboard: hold Enter · Panel: long-press select again"
              : "Gamepad: hold the button · Keyboard: hold Enter · Panel: long-press select"}
          </p>
        </>
      }
    >
      {danger ? (
        <p className="mb-[0.6rem] text-[0.8rem] text-err" data-testid="stop-sheet-body">
          {stopSheetBody()}
        </p>
      ) : null}
      {skill.takesAltitude ? (
        <NumberSettingRow
          title="Takeoff altitude"
          value={altitudeM}
          onChange={setAltitude}
          min={LAUNCH.altMinM}
          max={LAUNCH.altMaxM}
          format={(m) => `${m} m`}
          decrementLabel="Lower takeoff altitude"
          incrementLabel="Raise takeoff altitude"
        />
      ) : null}
    </Sheet>
  );
}
