// The confirm sheet for every non-tap skill. One sheet, four input paths:
// press-and-hold (touch/pointer), slide-to-confirm (touch/pointer, slide
// tier), a held gamepad button or Enter (any tier), and a long-press of the
// panel's activate button. A guarded skill (kill) shows its armed guard and
// closes when the guard window lapses without a completed hold.

import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { Minus, Plus, ShieldAlert, X } from "lucide-react";

import { useSkillContext } from "@/hooks/use-skill-context";
import { executeSkill } from "@/lib/skill-runner";
import { holdMsFor, resolveSkillState } from "@/lib/skills";
import { SLIDE_START_MAX, slideCompletes } from "@/lib/slide";
import { cn } from "@/lib/utils";
import { useConfirmStore } from "@/stores/confirm-store";

const ALT_MIN_M = 2;
const ALT_MAX_M = 120;

export function SkillConfirmSheet() {
  const pending = useConfirmStore((s) => s.pending);
  if (!pending) return null;
  return <Sheet key={pending.skill.id} />;
}

function Sheet() {
  const pending = useConfirmStore((s) => s.pending)!;
  const guardUntil = useConfirmStore((s) => s.guardUntil);
  const heldSince = useConfirmStore((s) => s.heldSince);
  const panelNonce = useConfirmStore((s) => s.panelConfirmNonce);
  const cancel = useConfirmStore((s) => s.cancel);
  const setAltitude = useConfirmStore((s) => s.setAltitude);
  const setHeld = useConfirmStore((s) => s.setHeld);
  const ctx = useSkillContext();

  const { skill, altitudeM } = pending;
  const holdMs = holdMsFor(skill.gesture);
  const state = resolveSkillState(skill, ctx);
  const [pointerSince, setPointerSince] = useState<number | null>(null);
  const [progress, setProgress] = useState(0);
  const firedRef = useRef(false);
  const initialNonce = useRef(panelNonce);

  const fire = () => {
    if (firedRef.current) return;
    firedRef.current = true;
    const current = useConfirmStore.getState().pending;
    cancel();
    if (current) void executeSkill(current.skill, { altitudeM: current.altitudeM, active: state.active });
  };

  // The skill stopped being drivable while the sheet was up: close it.
  useEffect(() => {
    if (!state.enabled) cancel();
  }, [state.enabled, cancel]);

  // A guard that lapses without a completed hold disarms the sheet.
  useEffect(() => {
    if (guardUntil === null) return;
    const id = setTimeout(() => {
      if (useConfirmStore.getState().heldSince === null) cancel();
    }, Math.max(0, guardUntil - performance.now()));
    return () => clearTimeout(id);
  }, [guardUntil, cancel]);

  // A panel long-press is a completed hold in one gesture.
  useEffect(() => {
    if (panelNonce !== initialNonce.current) fire();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [panelNonce]);

  // Enter held confirms; Escape cancels.
  useEffect(() => {
    const down = (e: KeyboardEvent) => {
      if (e.key === "Escape") cancel();
      else if (e.key === "Enter" && !e.repeat) setHeld(true);
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

  const holdHandlers = {
    onPointerDown: (e: ReactPointerEvent) => {
      e.currentTarget.setPointerCapture?.(e.pointerId);
      setPointerSince(performance.now());
    },
    onPointerUp: () => setPointerSince(null),
    onPointerCancel: () => setPointerSince(null),
    onPointerLeave: () => setPointerSince(null),
  };

  const danger = skill.gesture === "guarded";
  const holdLabel = `Hold to confirm (${(holdMs / 1000).toFixed(1)} s)`;

  return (
    <div className="pointer-events-auto absolute inset-0 z-50 flex items-end justify-center bg-scrim pb-[1rem]">
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="skill-confirm-title"
        className={cn(
          "w-[min(94%,30rem)] rounded-xl border bg-hud-glass-strong p-[0.9rem] text-hud-ink backdrop-blur-hud",
          danger ? "border-err" : "border-hud-hair",
        )}
      >
        <div className="mb-[0.6rem] flex items-center justify-between gap-[0.6rem]">
          <h2 id="skill-confirm-title" className="flex items-center gap-[0.4rem] text-[1rem] font-semibold">
            {danger ? <ShieldAlert className="h-[1.2rem] w-[1.2rem] text-err" aria-hidden /> : null}
            {skill.label}
          </h2>
          <button
            type="button"
            onClick={cancel}
            aria-label="Cancel"
            className="touch-target flex items-center justify-center rounded-md text-hud-ink-2 hover:bg-hud-hair"
          >
            <X className="h-[1.2rem] w-[1.2rem]" aria-hidden />
          </button>
        </div>

        {danger ? (
          <p className="mb-[0.6rem] text-[0.8rem] text-err">
            Guard armed. Motors stop immediately and an airborne vehicle falls.
          </p>
        ) : null}

        {skill.takesAltitude ? (
          <div className="mb-[0.7rem] flex items-center justify-between gap-[0.6rem]">
            <span className="text-[0.8rem] text-hud-ink-2">Takeoff altitude</span>
            <div className="flex items-center gap-[0.4rem]">
              <button
                type="button"
                aria-label="Lower takeoff altitude"
                onClick={() => setAltitude(Math.max(ALT_MIN_M, altitudeM - 1))}
                className="touch-target flex items-center justify-center rounded-md bg-hud-hair-2"
              >
                <Minus className="h-[1rem] w-[1rem]" aria-hidden />
              </button>
              <span className="min-w-[4rem] text-center font-mono text-[1rem] tabular-nums" aria-live="polite">
                {altitudeM} m
              </span>
              <button
                type="button"
                aria-label="Raise takeoff altitude"
                onClick={() => setAltitude(Math.min(ALT_MAX_M, altitudeM + 1))}
                className="touch-target flex items-center justify-center rounded-md bg-hud-hair-2"
              >
                <Plus className="h-[1rem] w-[1rem]" aria-hidden />
              </button>
            </div>
          </div>
        ) : null}

        {skill.gesture === "slide" ? (
          <>
            <SlideTrack onComplete={fire} label={skill.label} />
            {/* Gamepad / keyboard holds still complete the slide tier; show
                their progress without offering a pointer shortcut. */}
            <div className="mt-[0.5rem] h-[0.35rem] w-full overflow-hidden rounded-full bg-hud-hair-2" aria-hidden>
              <div className="h-full bg-primary" style={{ width: `${(since === null ? 0 : progress) * 100}%` }} />
            </div>
          </>
        ) : (
          <button
            type="button"
            {...holdHandlers}
            className={cn(
              "relative mt-[0.5rem] flex min-h-[3.4rem] w-full touch-none select-none items-center justify-center overflow-hidden rounded-lg font-semibold",
              danger ? "bg-err/25 text-err" : "bg-primary/20 text-hud-ink",
            )}
          >
            <span
              className={cn("absolute inset-y-0 left-0", danger ? "bg-err/45" : "bg-primary/45")}
              style={{ width: `${(since === null ? 0 : progress) * 100}%` }}
              aria-hidden
            />
            <span className="relative text-[0.9rem]">{holdLabel}</span>
          </button>
        )}
        <p className="mt-[0.4rem] text-center text-[0.75rem] text-hud-ink-2">
          Gamepad: hold the button · Keyboard: hold Enter · Panel: long-press select
        </p>
      </div>
    </div>
  );
}

/** Slide-to-confirm: drag the thumb across the track. Releasing early snaps
 *  it back. */
function SlideTrack({ onComplete, label }: { onComplete: () => void; label: string }) {
  const trackRef = useRef<HTMLDivElement>(null);
  const [frac, setFrac] = useState(0);
  const startFrac = useRef<number | null>(null);

  const fracAt = (clientX: number) => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0) return 0;
    return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  };
  const reset = () => {
    startFrac.current = null;
    setFrac(0);
  };

  return (
    <div
      ref={trackRef}
      role="slider"
      aria-label={`Slide to ${label}`}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(frac * 100)}
      className="relative h-[3.4rem] w-full touch-none select-none rounded-lg bg-hud-hair-2"
      onPointerDown={(e) => {
        // A drag counts only when it starts on the thumb; pressing never confirms.
        const f = fracAt(e.clientX);
        if (f >= SLIDE_START_MAX) return;
        startFrac.current = f;
        e.currentTarget.setPointerCapture?.(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (startFrac.current === null) return;
        const f = fracAt(e.clientX);
        setFrac(f);
        if (slideCompletes(startFrac.current, f)) {
          startFrac.current = null;
          onComplete();
        }
      }}
      onPointerUp={reset}
      onPointerCancel={reset}
    >
      <span className="absolute inset-0 flex items-center justify-center text-[0.85rem] text-hud-ink-2">
        Slide to {label.toLowerCase()} →
      </span>
      <span
        className="absolute inset-y-[0.3rem] w-[3rem] rounded-md bg-primary"
        style={{ left: `calc(0.3rem + ${frac} * (100% - 3.6rem))` }}
        aria-hidden
      />
    </div>
  );
}
