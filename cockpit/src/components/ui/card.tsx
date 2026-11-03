// A launch card for console surfaces (the quick menu, a profile's home): an
// icon, a title and a subtitle. `emphasis` makes the hero card larger with an
// accent icon. A disabled card stays focusable and shows its reason, so a
// controller user can still read why it is unavailable. Mirrors the Android
// console card.

import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

import { cn } from "@/lib/utils";

export interface CardProps {
  title: ReactNode;
  icon: LucideIcon;
  onClick: () => void;
  subtitle?: ReactNode;
  enabled?: boolean;
  disabledReason?: ReactNode;
  emphasis?: boolean;
  /** The card for the screen the operator is on. */
  current?: boolean;
}

export function Card({
  title,
  icon: Icon,
  onClick,
  subtitle,
  enabled = true,
  disabledReason,
  emphasis = false,
  current = false,
}: CardProps) {
  const secondary = !enabled && disabledReason != null ? disabledReason : subtitle;
  return (
    <button
      type="button"
      onClick={() => {
        if (enabled) onClick();
      }}
      aria-disabled={!enabled || undefined}
      aria-current={current ? "page" : undefined}
      className={cn(
        "touch-target flex flex-col items-start gap-[0.25rem] rounded-xl p-[0.7rem] text-left transition-transform duration-quick focus-visible:scale-[1.03] motion-reduce:transition-none",
        emphasis ? "min-h-[6.5rem]" : "min-h-[4.5rem]",
        current ? "bg-primary text-primary-foreground" : "bg-muted text-surface-foreground hover:bg-border",
      )}
    >
      <Icon
        className={cn(
          emphasis ? "h-[2rem] w-[2rem]" : "h-[1.5rem] w-[1.5rem]",
          emphasis && enabled && !current && "text-primary",
          !enabled && "opacity-50",
        )}
        aria-hidden
      />
      <span className={cn("mt-auto truncate font-medium", emphasis ? "text-[1.05rem]" : "text-[0.85rem]", !enabled && "opacity-50")}>
        {title}
      </span>
      {secondary != null ? (
        <span className={cn("line-clamp-2 text-[0.75rem]", !enabled ? "text-warn" : current ? "opacity-80" : "text-muted-foreground")}>
          {secondary}
        </span>
      ) : null}
    </button>
  );
}
