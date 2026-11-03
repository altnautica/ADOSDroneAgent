// A list row: a headline, an optional supporting line, a leading icon and a
// trailing slot. A real button (focusable, clickable) when `onClick` is set,
// a static row otherwise; `selected` adds the accent bar at the leading edge
// and a disabled row stays readable at reduced emphasis. Mirrors the Android
// console list item.

import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

import { cn } from "@/lib/utils";

export interface ListItemProps {
  headline: ReactNode;
  supporting?: ReactNode;
  icon?: LucideIcon;
  trailing?: ReactNode;
  selected?: boolean;
  enabled?: boolean;
  onClick?: () => void;
}

export function ListItem({
  headline,
  supporting,
  icon: Icon,
  trailing,
  selected = false,
  enabled = true,
  onClick,
}: ListItemProps) {
  const body = (
    <>
      {selected ? <span className="absolute inset-y-0 left-0 w-[3px] rounded-l-md bg-primary" aria-hidden /> : null}
      {Icon ? <Icon className="h-[1.3rem] w-[1.3rem] shrink-0 text-muted-foreground" aria-hidden /> : null}
      <span className="min-w-0 flex-1">
        <span className={cn("block truncate text-[0.95rem]", selected ? "text-primary" : "text-surface-foreground")}>
          {headline}
        </span>
        {supporting != null ? (
          <span className="block truncate text-[0.75rem] text-muted-foreground">{supporting}</span>
        ) : null}
      </span>
      {trailing}
    </>
  );
  const base = cn(
    "touch-target relative flex w-full items-center gap-[0.6rem] rounded-md px-[0.75rem] py-[0.4rem] text-left",
    selected && "bg-primary/10",
    !enabled && "opacity-50",
  );
  if (!onClick) return <div className={base}>{body}</div>;
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!enabled}
      aria-current={selected ? "true" : undefined}
      className={cn(base, "hover:bg-muted active:bg-muted")}
    >
      {body}
    </button>
  );
}
