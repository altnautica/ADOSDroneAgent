// An integer stepper row. Holding a ± button repeats after 400 ms every 80 ms,
// at five steps a tick after 1.5 s; Left/Right step while the row has focus;
// tapping the value opens typed entry on the on-screen numpad, checked against
// the range. Mirrors the Android NumberSettingRow.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Minus, Plus } from "lucide-react";

import { Dialog } from "@/components/ui/dialog";
import { SettingRow } from "@/components/ui/setting-row";
import { OnScreenNumpad } from "@/components/settings/on-screen-numpad";
import { cn } from "@/lib/utils";

const REPEAT_DELAY_MS = 400;
const REPEAT_INTERVAL_MS = 80;
/** Repeats before the fast rate: the ticks that fit in the first 1.5 s. */
const FAST_AFTER_TICKS = Math.ceil((1500 - REPEAT_DELAY_MS) / REPEAT_INTERVAL_MS);
const FAST_MULTIPLIER = 5;

export interface NumberSettingRowProps {
  title: string;
  value: number;
  onChange: (next: number) => void;
  min: number;
  max: number;
  step?: number;
  summary?: ReactNode;
  enabled?: boolean;
  format?: (value: number) => string;
  decrementLabel?: string;
  incrementLabel?: string;
  /** Typed-entry dialog wording. */
  entryConfirmLabel?: string;
  entryCancelLabel?: string;
}

export function NumberSettingRow({
  title,
  value,
  onChange,
  min,
  max,
  step = 1,
  summary,
  enabled = true,
  format = String,
  decrementLabel = "Decrease",
  incrementLabel = "Increase",
  entryConfirmLabel = "Set",
  entryCancelLabel = "Cancel",
}: NumberSettingRowProps) {
  const valueRef = useRef(value);
  const changeRef = useRef(onChange);
  useEffect(() => {
    valueRef.current = value;
    changeRef.current = onChange;
  });
  const repeat = useRef<{ timer: ReturnType<typeof setTimeout> } | null>(null);
  const [entry, setEntry] = useState<string | null>(null);

  const stepBy = (delta: number) => {
    const next = Math.min(max, Math.max(min, valueRef.current + delta));
    if (next === valueRef.current) return false;
    valueRef.current = next;
    changeRef.current(next);
    return true;
  };
  const stopRepeat = () => {
    if (repeat.current) clearTimeout(repeat.current.timer);
    repeat.current = null;
  };
  const startRepeat = (dir: 1 | -1) => {
    stopRepeat();
    if (!stepBy(dir * step)) return;
    let ticks = 0;
    const tick = () => {
      ticks += 1;
      const fast = ticks > FAST_AFTER_TICKS;
      if (!stepBy(dir * step * (fast ? FAST_MULTIPLIER : 1))) {
        stopRepeat();
        return;
      }
      repeat.current = { timer: setTimeout(tick, REPEAT_INTERVAL_MS) };
    };
    repeat.current = { timer: setTimeout(tick, REPEAT_DELAY_MS) };
  };
  // A held step stops when the row goes away.
  useEffect(() => {
    const held = repeat;
    return () => {
      if (held.current) clearTimeout(held.current.timer);
    };
  }, []);

  const parsed = entry === null ? null : Number(entry);
  const entryValid = parsed !== null && entry !== "" && Number.isInteger(parsed) && parsed >= min && parsed <= max;

  const stepButton = (dir: 1 | -1) => (
    <button
      type="button"
      disabled={!enabled || (dir < 0 ? value <= min : value >= max)}
      aria-label={dir < 0 ? decrementLabel : incrementLabel}
      onPointerDown={() => startRepeat(dir)}
      onPointerUp={stopRepeat}
      onPointerLeave={stopRepeat}
      onPointerCancel={stopRepeat}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          stepBy(dir * step);
        }
      }}
      className="touch-target flex items-center justify-center rounded-md bg-muted text-surface-foreground hover:bg-border disabled:opacity-40"
    >
      {dir < 0 ? <Minus className="h-[1.1rem] w-[1.1rem]" aria-hidden /> : <Plus className="h-[1.1rem] w-[1.1rem]" aria-hidden />}
    </button>
  );

  return (
    <SettingRow title={title} summary={summary} enabled={enabled}>
      <div
        className="flex items-center gap-[0.3rem]"
        onKeyDown={(e) => {
          if (!enabled) return;
          if (e.key === "ArrowLeft") {
            if (stepBy(-step)) e.preventDefault();
          } else if (e.key === "ArrowRight") {
            if (stepBy(step)) e.preventDefault();
          }
        }}
      >
        {stepButton(-1)}
        <button
          type="button"
          disabled={!enabled}
          aria-label={`${title}: ${format(value)}`}
          onClick={() => setEntry(String(value))}
          className="touch-target min-w-[4rem] rounded-md px-[0.4rem] text-center font-mono text-[1rem] text-surface-foreground hover:bg-muted"
        >
          {format(value)}
        </button>
        {stepButton(1)}
      </div>
      {entry !== null ? (
        <Dialog
          title={title}
          onDismiss={() => setEntry(null)}
          dismiss={
            <button
              type="button"
              onClick={() => setEntry(null)}
              className="touch-target rounded-lg bg-muted px-[0.9rem] text-[0.85rem] text-surface-foreground"
            >
              {entryCancelLabel}
            </button>
          }
          confirm={
            <button
              type="button"
              disabled={!entryValid}
              onClick={() => {
                if (parsed === null) return;
                changeRef.current(parsed);
                setEntry(null);
              }}
              className={cn(
                "touch-target rounded-lg px-[0.9rem] text-[0.85rem] font-medium",
                entryValid ? "bg-primary text-primary-foreground" : "bg-muted text-muted-foreground",
              )}
            >
              {entryConfirmLabel}
            </button>
          }
        >
          <OnScreenNumpad value={entry} onChange={setEntry} bound={{ min, max, step, int: true }} />
        </Dialog>
      ) : null}
    </SettingRow>
  );
}
