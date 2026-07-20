// Shared presentational primitives for the status screens (Link / Mesh / Pair /
// Uplink / System). Glanceable 48px rows and tiles, a semantic status dot, a
// percent bar, and touch action buttons — the amber-on-charcoal house grammar.
// Every interactive element floors its hit target at 48px (the resistive-panel
// rule) and is a real <button> so touch and the shared focus ring both drive it.

import { useEffect, useRef, useState, type ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

import { cn } from "@/lib/utils";

/** A semantic status colour keyed off a link/service state string. */
export type Tone = "ok" | "warn" | "err" | "muted";

export function toneClass(tone: Tone): string {
  switch (tone) {
    case "ok":
      return "text-ok";
    case "warn":
      return "text-warn";
    case "err":
      return "text-err";
    default:
      return "text-muted-foreground";
  }
}

function toneDotClass(tone: Tone): string {
  switch (tone) {
    case "ok":
      return "bg-ok";
    case "warn":
      return "bg-warn";
    case "err":
      return "bg-err";
    default:
      return "bg-muted-foreground";
  }
}

/** A small round status dot. */
export function Dot({ tone }: { tone: Tone }) {
  return (
    <span
      className={cn("h-[0.6rem] w-[0.6rem] shrink-0 rounded-full", toneDotClass(tone))}
      aria-hidden
    />
  );
}

/** A dim uppercase section label separating groups of rows/tiles. */
export function SectionHeader({ children }: { children: ReactNode }) {
  return (
    <h2 className="mb-[0.3rem] mt-[0.7rem] px-[0.15rem] text-[0.66rem] font-medium uppercase tracking-wider text-muted-foreground first:mt-0">
      {children}
    </h2>
  );
}

/** A glanceable full-width 48px row: label on the left, value on the right. When
 *  `onClick` is set it is a focusable button; otherwise a static row. `tone`
 *  colours the value; `left` renders a leading element (a status dot). */
export function Row({
  label,
  value,
  hint,
  tone,
  left,
  onClick,
  mono = true,
}: {
  label: ReactNode;
  value?: ReactNode;
  hint?: ReactNode;
  tone?: Tone;
  left?: ReactNode;
  onClick?: () => void;
  mono?: boolean;
}) {
  const inner = (
    <>
      <div className="flex min-w-0 items-center gap-[0.5rem]">
        {left}
        <div className="min-w-0">
          <div className="truncate text-[0.85rem] text-surface-foreground">{label}</div>
          {hint != null ? (
            <div className="truncate text-[0.68rem] text-muted-foreground">{hint}</div>
          ) : null}
        </div>
      </div>
      {value != null ? (
        <span
          className={cn(
            "shrink-0 text-right text-[0.9rem]",
            mono && "font-mono",
            tone ? toneClass(tone) : "text-surface-foreground",
          )}
        >
          {value}
        </span>
      ) : null}
    </>
  );

  const base =
    "touch-target flex w-full items-center justify-between gap-[0.6rem] rounded-md px-[0.6rem] py-[0.4rem]";

  if (onClick) {
    return (
      <button type="button" onClick={onClick} className={cn(base, "bg-input/50 hover:bg-muted active:bg-muted")}>
        {inner}
      </button>
    );
  }
  return <div className={cn(base, "bg-input/30")}>{inner}</div>;
}

/** A stat tile for a compact grid — a big value over a small label. */
export function Tile({
  label,
  value,
  tone,
  hint,
}: {
  label: string;
  value: ReactNode;
  tone?: Tone;
  hint?: ReactNode;
}) {
  return (
    <div className="flex min-h-[3.4rem] flex-col justify-center rounded-md bg-input/40 px-[0.6rem] py-[0.4rem]">
      <span className="text-[0.6rem] uppercase tracking-wide text-muted-foreground">{label}</span>
      <span
        className={cn(
          "font-mono text-[1.05rem] font-semibold leading-tight",
          tone ? toneClass(tone) : "text-surface-foreground",
        )}
      >
        {value}
      </span>
      {hint != null ? (
        <span className="text-[0.62rem] text-muted-foreground">{hint}</span>
      ) : null}
    </div>
  );
}

/** A responsive tile grid (two columns on the reference panel, three when wide). */
export function TileGrid({ children }: { children: ReactNode }) {
  return (
    <div className="grid grid-cols-2 gap-[0.4rem] landscape:grid-cols-3">{children}</div>
  );
}

/** A thin percent bar (cpu / ram / disk / data-cap). `value`/`max` are absolute;
 *  the fill colour steps ok → warn → err as it fills. */
export function ProgressBar({
  value,
  max = 100,
  tone,
}: {
  value: number | null | undefined;
  max?: number;
  tone?: Tone;
}) {
  const pct =
    value == null || !Number.isFinite(value) || max <= 0
      ? null
      : Math.max(0, Math.min(100, (value / max) * 100));
  const autoTone: Tone = pct == null ? "muted" : pct >= 90 ? "err" : pct >= 75 ? "warn" : "ok";
  const fill = toneDotClass(tone ?? autoTone);
  return (
    <div className="h-[0.4rem] w-full overflow-hidden rounded-full bg-input">
      {pct != null ? (
        <div className={cn("h-full rounded-full transition-[width]", fill)} style={{ width: `${pct}%` }} />
      ) : null}
    </div>
  );
}

/** A metered tile: a label, a value, and a fill bar under it. */
export function MeterTile({
  label,
  value,
  max,
  display,
  tone,
}: {
  label: string;
  value: number | null | undefined;
  max?: number;
  display: ReactNode;
  tone?: Tone;
}) {
  return (
    <div className="flex min-h-[3.4rem] flex-col justify-center gap-[0.3rem] rounded-md bg-input/40 px-[0.6rem] py-[0.4rem]">
      <div className="flex items-baseline justify-between">
        <span className="text-[0.6rem] uppercase tracking-wide text-muted-foreground">{label}</span>
        <span className={cn("font-mono text-[0.8rem]", tone ? toneClass(tone) : "text-surface-foreground")}>
          {display}
        </span>
      </div>
      <ProgressBar value={value} max={max} tone={tone} />
    </div>
  );
}

/** A large touch action button. `variant="danger"` is the destructive style. */
export function ActionButton({
  label,
  icon: Icon,
  onClick,
  variant = "default",
  busy = false,
  disabled = false,
  full = false,
}: {
  label: string;
  icon?: LucideIcon;
  onClick: () => void;
  variant?: "default" | "primary" | "danger";
  busy?: boolean;
  disabled?: boolean;
  full?: boolean;
}) {
  const style =
    variant === "danger"
      ? "bg-err/20 text-err hover:bg-err/30 active:bg-err/40"
      : variant === "primary"
        ? "bg-amber text-amber-foreground hover:brightness-95 active:brightness-90"
        : "bg-input text-surface-foreground hover:bg-muted active:bg-muted";
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled || busy}
      className={cn(
        "touch-target flex items-center justify-center gap-[0.4rem] rounded-lg px-[0.9rem] text-[0.85rem] font-medium transition disabled:opacity-50",
        full && "w-full",
        style,
      )}
    >
      {Icon ? <Icon className={cn("h-[1.1rem] w-[1.1rem]", busy && "animate-spin")} aria-hidden /> : null}
      {label}
    </button>
  );
}

/** A two-tap confirm button for destructive actions (unpair, service restart).
 *  First tap arms it ("Confirm?") for a few seconds; a second tap within the
 *  window fires. No modal/portal — field-friendly, operable by touch or button.
 *  While armed it turns danger-red so the operator sees the state. */
export function ConfirmButton({
  label,
  confirmLabel = "Confirm?",
  icon: Icon,
  onConfirm,
  busy = false,
  disabled = false,
  full = false,
  compact = false,
  windowMs = 3500,
}: {
  label: string;
  confirmLabel?: string;
  icon?: LucideIcon;
  onConfirm: () => void;
  busy?: boolean;
  disabled?: boolean;
  full?: boolean;
  /** Render as a small square icon-only button (per-row restart controls). */
  compact?: boolean;
  windowMs?: number;
}) {
  const [armed, setArmed] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => () => {
    if (timer.current) clearTimeout(timer.current);
  }, []);

  const disarm = () => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    setArmed(false);
  };

  const handle = () => {
    if (armed) {
      disarm();
      onConfirm();
    } else {
      setArmed(true);
      timer.current = setTimeout(() => setArmed(false), windowMs);
    }
  };

  if (compact) {
    return (
      <button
        type="button"
        onClick={handle}
        disabled={disabled || busy}
        aria-label={armed ? confirmLabel : label}
        title={armed ? confirmLabel : label}
        className={cn(
          "touch-target flex items-center justify-center rounded-md transition disabled:opacity-50",
          armed ? "bg-err/25 text-err ring-1 ring-err" : "bg-input text-muted-foreground hover:bg-muted active:bg-muted",
        )}
      >
        {Icon ? <Icon className={cn("h-[1.1rem] w-[1.1rem]", busy && "animate-spin")} aria-hidden /> : "?"}
      </button>
    );
  }

  return (
    <ActionButton
      label={armed ? confirmLabel : label}
      icon={Icon}
      onClick={handle}
      variant={armed ? "danger" : "default"}
      busy={busy}
      disabled={disabled}
      full={full}
    />
  );
}

/** A small right-aligned header badge that reads "stale" when a poll is failing
 *  (the honest-surface signal — the data may be old). */
export function StaleBadge({ stale }: { stale?: boolean }) {
  if (stale) return <span className="text-[0.68rem] text-warn">stale</span>;
  return null;
}

/** A centred empty/placeholder note for a screen with no data yet or a route
 *  that is not available on this profile. */
export function EmptyNote({ children }: { children: ReactNode }) {
  return (
    <div className="flex flex-1 items-center justify-center px-[1rem] text-center text-[0.82rem] text-muted-foreground">
      {children}
    </div>
  );
}
