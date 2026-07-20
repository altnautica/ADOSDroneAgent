// Touch widgets for the Settings editors: a big switch for booleans and a
// large-row picker for the constrained (`Literal`/enum) fields. Both floor
// their hit targets at 48px for the resistive panel and carry the shared focus
// ring, so touch and the button/gamepad ring drive them identically.

import { Check } from "lucide-react";

import { cn } from "@/lib/utils";

/** A large on/off switch. Tapping the whole control flips it. */
export function Toggle({
  on,
  onChange,
  label,
  disabled = false,
}: {
  on: boolean;
  onChange: (next: boolean) => void;
  label?: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!on)}
      className={cn(
        "touch-target relative inline-flex w-[3.6rem] shrink-0 items-center rounded-full px-[0.2rem] transition-colors disabled:opacity-50",
        on ? "bg-amber" : "bg-input",
      )}
    >
      <span
        className={cn(
          "h-[1.5rem] w-[1.5rem] rounded-full bg-background shadow transition-transform",
          on ? "translate-x-[2rem]" : "translate-x-0",
        )}
      />
    </button>
  );
}

/** A vertical list of large option rows for a constrained value. The selected
 *  option is amber with a check; tapping one selects it (the editor's Save
 *  commits). Scrolls when the option set is long. */
export function SegmentedPicker({
  options,
  value,
  onSelect,
  optionLabel,
}: {
  options: string[];
  value: string | null;
  onSelect: (option: string) => void;
  /** Optional prettier label for an option (the raw value is still the value). */
  optionLabel?: (option: string) => string;
}) {
  return (
    <div className="flex flex-col gap-[0.3rem]">
      {options.map((opt) => {
        const selected = opt === value;
        return (
          <button
            key={opt}
            type="button"
            aria-pressed={selected}
            onClick={() => onSelect(opt)}
            className={cn(
              "touch-target flex items-center justify-between gap-[0.5rem] rounded-lg px-[0.9rem] text-left transition-colors",
              selected
                ? "bg-amber text-amber-foreground"
                : "bg-input text-surface-foreground hover:bg-muted active:bg-muted",
            )}
          >
            <span className="min-w-0 truncate font-mono text-[0.95rem]">
              {optionLabel ? optionLabel(opt) : opt}
            </span>
            {selected ? <Check className="h-[1.2rem] w-[1.2rem] shrink-0" aria-hidden /> : null}
          </button>
        );
      })}
    </div>
  );
}
