import { Check } from "lucide-react";

import { cn } from "@/lib/utils";

interface RadioCardOption<T extends string> {
  value: T;
  label: string;
  description?: string;
  disabled?: boolean;
  badge?: string;
}

interface RadioCardGroupProps<T extends string> {
  value: T | null;
  onChange: (next: T) => void;
  options: ReadonlyArray<RadioCardOption<T>>;
  className?: string;
  columns?: 1 | 2 | 3;
}

export function RadioCardGroup<T extends string>({
  value,
  onChange,
  options,
  className,
  columns = 1,
}: RadioCardGroupProps<T>) {
  const cols = columns === 3 ? "lg:grid-cols-3" : columns === 2 ? "lg:grid-cols-2" : "";
  return (
    <div className={cn("grid gap-2", cols, className)} role="radiogroup">
      {options.map((opt) => {
        const selected = value === opt.value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={selected}
            disabled={opt.disabled}
            onClick={() => !opt.disabled && onChange(opt.value)}
            className={cn(
              "text-left rounded-md border px-3 py-2.5 transition-colors focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring",
              selected
                ? "border-primary bg-primary/10 text-foreground"
                : "border-border bg-card hover:border-border/80 hover:bg-accent/30",
              opt.disabled && "opacity-50 cursor-not-allowed",
            )}
          >
            <div className="flex items-center justify-between gap-2 mb-0.5">
              <span className="font-medium text-sm">{opt.label}</span>
              <div className="flex items-center gap-2">
                {opt.badge && (
                  <span className="text-[10px] font-medium uppercase tracking-wider text-info">
                    {opt.badge}
                  </span>
                )}
                {selected && <Check className="h-3.5 w-3.5 text-primary" />}
              </div>
            </div>
            {opt.description && (
              <p className="text-xs text-muted-foreground leading-snug">
                {opt.description}
              </p>
            )}
          </button>
        );
      })}
    </div>
  );
}
