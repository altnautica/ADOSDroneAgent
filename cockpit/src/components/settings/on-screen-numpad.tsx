// A big-key on-screen numeric pad for number config leaves. Honors the field's
// bounds: an integer field hides the decimal point, a field that cannot go
// negative hides the sign key, and the min..max range is shown so the operator
// sees the legal window (the agent still validates server-side, but the pad
// never offers an obviously-illegal keystroke). Controlled — the editor owns
// the string buffer.

import { Delete, X } from "lucide-react";

import type { NumberBound } from "@/lib/settings-schema";
import { cn } from "@/lib/utils";

function Key({
  children,
  onClick,
  className,
  ariaLabel,
  disabled = false,
}: {
  children: React.ReactNode;
  onClick: () => void;
  className?: string;
  ariaLabel?: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={ariaLabel}
      className={cn(
        "touch-target flex items-center justify-center rounded-md bg-input text-[1.25rem] font-medium text-surface-foreground transition-colors hover:bg-muted active:bg-amber active:text-amber-foreground disabled:opacity-40",
        className,
      )}
    >
      {children}
    </button>
  );
}

export function OnScreenNumpad({
  value,
  onChange,
  bound,
}: {
  value: string;
  onChange: (next: string) => void;
  bound?: NumberBound;
}) {
  const isInt = bound?.int ?? false;
  const allowSign = bound?.min == null || bound.min < 0;

  const append = (ch: string) => {
    if (ch === "." && (isInt || value.includes("."))) return;
    if (ch === "-") {
      // Toggle a leading minus rather than appending mid-number.
      onChange(value.startsWith("-") ? value.slice(1) : `-${value}`);
      return;
    }
    onChange(value + ch);
  };
  const backspace = () => onChange(value.slice(0, -1));
  const clear = () => onChange("");

  const rangeHint =
    bound && (bound.min != null || bound.max != null)
      ? `range ${bound.min ?? "−∞"} … ${bound.max ?? "∞"}${isInt ? " · integer" : ""}`
      : isInt
        ? "integer"
        : null;

  return (
    <div className="mx-auto flex w-full max-w-[22rem] flex-col gap-[0.4rem]">
      <div className="flex min-h-[2.8rem] items-center justify-end rounded-md bg-input/40 px-[0.8rem] py-[0.5rem] font-mono text-[1.5rem] text-surface-foreground">
        {value || <span className="text-muted-foreground">0</span>}
      </div>
      {rangeHint ? (
        <div className="text-center text-[0.68rem] text-muted-foreground">{rangeHint}</div>
      ) : null}

      <div className="grid grid-cols-3 gap-[0.4rem]">
        {["7", "8", "9", "4", "5", "6", "1", "2", "3"].map((d) => (
          <Key key={d} onClick={() => append(d)}>
            {d}
          </Key>
        ))}
        <Key onClick={() => append("-")} disabled={!allowSign} ariaLabel="Toggle sign">
          ±
        </Key>
        <Key onClick={() => append("0")}>0</Key>
        <Key onClick={() => append(".")} disabled={isInt} ariaLabel="Decimal point">
          .
        </Key>
      </div>

      <div className="flex gap-[0.4rem]">
        <Key onClick={backspace} ariaLabel="Backspace" className="flex-1">
          <Delete className="h-[1.3rem] w-[1.3rem]" />
        </Key>
        <Key onClick={clear} ariaLabel="Clear" className="flex-1">
          <X className="h-[1.3rem] w-[1.3rem]" />
        </Key>
      </div>
    </div>
  );
}
