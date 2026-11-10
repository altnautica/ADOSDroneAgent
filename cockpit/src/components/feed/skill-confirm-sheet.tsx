// The confirm sheet for every non-tap skill, on the shared sheet. One sheet,
// several input paths: press-and-hold (touch/pointer), a held controller
// button or Enter, and a long-press of the panel's activate button. The hold
// time is the spec's (`builtins.json` `confirm.ms`); a `hold_confirm` skill
// (Emergency stop) asks for an explicit confirm once its hold completes.

import { useEffect, useRef, useState } from "react";
import { ShieldAlert } from "lucide-react";

import { NumberSettingRow } from "@/components/ui/number-setting-row";
import { Sheet } from "@/components/ui/sheet";
import { useSkillContext } from "@/hooks/use-skill-context";
import { LAUNCH } from "@/lib/cockpit-flow";
import { executeSkill } from "@/lib/skill-runner";
import { resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { useConfirmStore } from "@/stores/confirm-store";

export function SkillConfirmSheet() {
  const pending = useConfirmStore((s) => s.pending);
  if (!pending) return null;
  return <ConfirmSheet key={pending.skill.id} />;
}

function ConfirmSheet() {
  const pending = useConfirmStore((s) => s.pending)!;
  const toConfirmStage = useConfirmStore((s) => s.toConfirmStage);
  const heldSince = useConfirmStore((s) => s.heldSince);
  const panelNonce = useConfirmStore((s) => s.panelConfirmNonce);
  const cancel = useConfirmStore((s) => s.cancel);
  const setAltitude = useConfirmStore((s) => s.setAltitude);
  const setHeld = useConfirmStore((s) => s.setHeld);
  const ctx = useSkillContext();

  const { skill, altitudeM, stage } = pending;
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
    if (skill.confirm.kind === "hold_confirm" && stage === "hold") {
      toConfirmStage();
      firedRef.current = false;
      setPointerSince(null);
      return;
    }
    execute();
  };

  // The skill stopped being drivable while the sheet was up: close it.
  useEffect(() => {
    if (!state.enabled) cancel();
  }, [state.enabled, cancel]);


  // A panel long-press is a completed hold in one gesture.
  useEffect(() => {
    if (panelNonce !== initialNonce.current) fire();
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

  const shownProgress = since === null ? 0 : progress;
  const danger = skill.confirm.kind === "hold_confirm";

  const confirmControl =
    stage === "confirm" ? (
      <button
        type="button"
        onClick={execute}
        data-testid="confirm-final"
        className="flex min-h-[3.4rem] w-full items-center justify-center rounded-lg bg-err/70 text-[0.95rem] font-semibold text-on-status"
      >
        {`Confirm: ${skill.label}`}
      </button>
    ) : (
      <button
        type="button"
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
        <span className="relative text-[0.9rem]">{`Hold to confirm (${(holdMs / 1000).toFixed(1)} s)`}</span>
      </button>
    );

  return (
    <Sheet
      title={
        <span className="flex items-center gap-[0.4rem]">
          {danger ? <ShieldAlert className="h-[1.2rem] w-[1.2rem] text-err" aria-hidden /> : null}
          {skill.label}
        </span>
      }
      onDismiss={cancel}
      danger={danger}
      initialFocus={confirmRef}
      closeLabel="Cancel"
      hints={[
        { command: "activate", label: "Hold to confirm" },
        { command: "back", label: "Cancel" },
      ]}
      footer={
        <>
          <div ref={confirmRef}>{confirmControl}</div>
          <p className="mt-[0.4rem] text-center text-[0.75rem] text-muted-foreground">
            Gamepad: hold the button · Keyboard: hold Enter · Panel: long-press select
          </p>
        </>
      }
    >
      {danger ? (
        <p className="mb-[0.6rem] text-[0.8rem] text-err">
          Motors stop immediately and an airborne vehicle falls.
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
