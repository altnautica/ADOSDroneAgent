// The setup wizard's small controls: the action button in its three styles,
// the hold-to-confirm button, the progress ring and the step-state pill. Sizes
// are in em, so the host scales the whole wizard with one font size (×1.33 on
// a TV or kiosk); the focus ring shows on keyboard/D-pad focus, and on every
// focus when the wizard root carries `data-focus-rings="always"`.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Check, CircleDashed, Hand, Loader2, Minus, X } from "lucide-react";

import { setupText } from "./strings";
import { HOLD_CONFIRM_MS, type ActionStyle, type StepState } from "./types";

export const FOCUS_RING =
  "outline-none focus-visible:outline focus-visible:outline-[0.15em] focus-visible:outline-offset-[0.1em] focus-visible:outline-ring " +
  "[[data-focus-rings=always]_&]:focus:outline [[data-focus-rings=always]_&]:focus:outline-[0.15em] " +
  "[[data-focus-rings=always]_&]:focus:outline-offset-[0.1em] [[data-focus-rings=always]_&]:focus:outline-ring";

const STYLE_CLASS: Record<ActionStyle, string> = {
  primary: "bg-primary text-primary-foreground hover:bg-primary-hover",
  secondary: "border border-border-strong bg-muted/40 text-foreground hover:bg-muted",
  danger: "bg-destructive text-destructive-foreground hover:bg-destructive/90",
};

/** Every wizard button: a 48 px floor at the base size, grown with the em scale. */
const BUTTON_BASE =
  "relative inline-flex min-h-[3.43em] min-w-[3.43em] items-center justify-center gap-[0.5em] overflow-hidden rounded-lg px-[1.1em] text-[1em] font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-50";

function actionButtonClass(style: ActionStyle): string {
  return `${BUTTON_BASE} ${STYLE_CLASS[style]} ${FOCUS_RING}`;
}

export function WizardButton({
  style,
  onClick,
  disabled,
  children,
  className = "",
}: {
  style: ActionStyle;
  onClick: () => void;
  disabled?: boolean;
  children: ReactNode;
  className?: string;
}) {
  return (
    <button type="button" onClick={onClick} disabled={disabled} className={`${actionButtonClass(style)} ${className}`}>
      {children}
    </button>
  );
}

/**
 * A button that fires only after being held for `HOLD_CONFIRM_MS`, by pointer
 * or by holding Enter/Space. Letting go early cancels. A plain click (a
 * gamepad select delivers one) never fires it; the fill and the hint show
 * that it needs a hold.
 */
export function HoldButton({
  style,
  label,
  hint,
  onConfirm,
  disabled,
}: {
  style: ActionStyle;
  label: string;
  hint: string;
  onConfirm: () => void;
  disabled?: boolean;
}) {
  const [progress, setProgress] = useState(0);
  const startedAt = useRef<number | null>(null);
  const frame = useRef<number | undefined>(undefined);
  const latest = useRef(onConfirm);
  useEffect(() => {
    latest.current = onConfirm;
  });

  const cancel = () => {
    startedAt.current = null;
    if (frame.current !== undefined) cancelAnimationFrame(frame.current);
    frame.current = undefined;
    setProgress(0);
  };

  // Timed from the animation frame's own timestamp (the performance clock).
  const tick = (frameTime: number) => {
    if (startedAt.current === null) return;
    const p = Math.min(1, (frameTime - startedAt.current) / HOLD_CONFIRM_MS);
    setProgress(p);
    if (p >= 1) {
      cancel();
      latest.current();
      return;
    }
    frame.current = requestAnimationFrame(tick);
  };

  const begin = () => {
    if (disabled || startedAt.current !== null) return;
    startedAt.current = performance.now();
    frame.current = requestAnimationFrame(tick);
  };

  useEffect(() => cancel, []);

  return (
    <button
      type="button"
      disabled={disabled}
      aria-label={`${label}. ${hint}`}
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture(e.pointerId);
        begin();
      }}
      onPointerUp={cancel}
      onPointerCancel={cancel}
      onKeyDown={(e) => {
        if ((e.key === "Enter" || e.key === " ") && !e.repeat) {
          e.preventDefault();
          begin();
        }
      }}
      onKeyUp={(e) => {
        if (e.key === "Enter" || e.key === " ") cancel();
      }}
      onBlur={cancel}
      onContextMenu={(e) => e.preventDefault()}
      className={`${actionButtonClass(style)} touch-none select-none flex-col !gap-[0.1em] py-[0.4em]`}
    >
      <span
        aria-hidden
        className="absolute inset-y-0 left-0 bg-foreground/20"
        style={{ width: `${progress * 100}%` }}
      />
      <span className="relative flex items-center gap-[0.4em]">
        <Hand className="h-[1.1em] w-[1.1em]" aria-hidden />
        {label}
      </span>
      <span className="relative text-[0.75em] opacity-80">{hint}</span>
    </button>
  );
}

/** A progress ring: `value` 0..1, or a spinning arc when null. */
export function ProgressRing({ value, size = "5em" }: { value: number | null; size?: string }) {
  const r = 42;
  const c = 2 * Math.PI * r;
  return (
    <div className="relative shrink-0" style={{ width: size, height: size }} role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={value === null ? undefined : Math.round(value * 100)}>
      <svg viewBox="0 0 100 100" className={`h-full w-full -rotate-90 ${value === null ? "animate-spin" : ""}`}>
        <circle cx="50" cy="50" r={r} fill="none" strokeWidth="9" className="stroke-muted" />
        <circle
          cx="50"
          cy="50"
          r={r}
          fill="none"
          strokeWidth="9"
          strokeLinecap="round"
          className="stroke-primary transition-[stroke-dashoffset] duration-quick"
          strokeDasharray={c}
          strokeDashoffset={value === null ? c * 0.7 : c * (1 - value)}
        />
      </svg>
      {value !== null ? (
        <span className="absolute inset-0 flex items-center justify-center font-mono text-[1.05em] font-semibold tabular-nums text-foreground">
          {Math.round(value * 100)}%
        </span>
      ) : null}
    </div>
  );
}

const STATE_TONE: Record<StepState, string> = {
  pending: "text-muted-foreground",
  running: "text-info",
  waiting_user: "text-warn",
  done: "text-ok",
  failed: "text-err",
  skipped: "text-muted-foreground",
};

/** The step-state icon, coloured by state. */
export function StateIcon({ state }: { state: StepState }) {
  const cls = `h-[1.15em] w-[1.15em] shrink-0 ${STATE_TONE[state]}`;
  switch (state) {
    case "running":
      return <Loader2 className={`${cls} animate-spin`} aria-hidden />;
    case "waiting_user":
      return <Hand className={cls} aria-hidden />;
    case "done":
      return <Check className={cls} aria-hidden />;
    case "failed":
      return <X className={cls} aria-hidden />;
    case "skipped":
      return <Minus className={cls} aria-hidden />;
    case "pending":
      return <CircleDashed className={cls} aria-hidden />;
  }
}

/** The step state as a coloured pill. */
export function StatePill({ state }: { state: StepState }) {
  return (
    <span className={`inline-flex items-center gap-[0.3em] rounded-full border border-current/30 px-[0.6em] py-[0.15em] text-[0.8em] font-medium ${STATE_TONE[state]}`}>
      <StateIcon state={state} />
      {setupText(`setup.state.${state}`)}
    </span>
  );
}
