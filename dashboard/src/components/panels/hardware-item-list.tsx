// Per-item hardware list. Used by the wizard's connectivity step
// and by the home-page hardware panel (v0.16.1). Rendering is
// deliberately compact: one row per item, severity dot, label,
// detail line, and an optional fix-hint chip when the item is
// not OK.

import { AlertTriangle, CheckCircle2, Circle, HelpCircle, X } from "lucide-react";

import { cn } from "@/lib/utils";
import type { HardwareItem, HardwareItemState } from "@/lib/types";

interface HardwareItemListProps {
  items: HardwareItem[];
  emptyText?: string;
  showOptionalSection?: boolean;
  className?: string;
}

const STATE_META: Record<
  HardwareItemState,
  { tone: string; Icon: typeof CheckCircle2; label: string }
> = {
  ok: { tone: "text-ok", Icon: CheckCircle2, label: "ok" },
  warning: { tone: "text-warn", Icon: AlertTriangle, label: "warning" },
  missing: { tone: "text-destructive", Icon: X, label: "missing" },
  checking: { tone: "text-muted-foreground", Icon: Circle, label: "checking" },
  unknown: { tone: "text-muted-foreground", Icon: HelpCircle, label: "unknown" },
};

export function HardwareItemList({
  items,
  emptyText = "No hardware detected.",
  showOptionalSection = true,
  className,
}: HardwareItemListProps) {
  if (!items || items.length === 0) {
    return (
      <p className={cn("text-sm text-muted-foreground", className)}>
        {emptyText}
      </p>
    );
  }

  if (!showOptionalSection) {
    return (
      <ul className={cn("space-y-1.5", className)}>
        {items.map((item) => (
          <HardwareItemRow key={item.id} item={item} />
        ))}
      </ul>
    );
  }

  const required = items.filter((i) => i.required);
  const optional = items.filter((i) => !i.required);

  return (
    <div className={cn("space-y-3", className)}>
      {required.length > 0 && (
        <Section title="Required" items={required} />
      )}
      {optional.length > 0 && (
        <Section title="Optional" items={optional} />
      )}
    </div>
  );
}

function Section({ title, items }: { title: string; items: HardwareItem[] }) {
  return (
    <div>
      <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">
        {title}
      </div>
      <ul className="space-y-1.5">
        {items.map((item) => (
          <HardwareItemRow key={item.id} item={item} />
        ))}
      </ul>
    </div>
  );
}

function HardwareItemRow({ item }: { item: HardwareItem }) {
  const { tone, Icon, label } = STATE_META[item.state];
  return (
    <li className="flex items-start gap-2 rounded border border-border/60 px-2.5 py-1.5">
      <Icon
        className={cn("h-4 w-4 mt-0.5 shrink-0", tone)}
        aria-label={label}
      />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 flex-wrap">
          <span className="text-sm font-medium">{item.label}</span>
          <span
            className={cn(
              "text-[10px] uppercase tracking-wider px-1 rounded border",
              item.state === "ok"
                ? "border-ok/40 text-ok"
                : item.state === "warning"
                  ? "border-warn/40 text-warn"
                  : item.state === "missing"
                    ? "border-destructive/40 text-destructive"
                    : "border-muted-foreground/40 text-muted-foreground",
            )}
          >
            {item.state}
          </span>
        </div>
        {item.detail && (
          <p className="text-xs text-muted-foreground leading-snug mt-0.5">
            {item.detail}
          </p>
        )}
        {item.fix_hint && item.state !== "ok" && (
          <p className="text-xs text-foreground/80 mt-1">
            <span className="font-medium">Fix:</span> {item.fix_hint}
          </p>
        )}
      </div>
    </li>
  );
}

// Helper for status tiles + summaries: returns counts split by required.
export function summarizeHardware(items: HardwareItem[]): {
  requiredOk: number;
  requiredTotal: number;
  optionalOk: number;
  optionalTotal: number;
  worstState: HardwareItemState;
} {
  let requiredOk = 0;
  let requiredTotal = 0;
  let optionalOk = 0;
  let optionalTotal = 0;
  const STATE_RANK: Record<HardwareItemState, number> = {
    ok: 0,
    unknown: 1,
    checking: 2,
    warning: 3,
    missing: 4,
  };
  let worstRank = 0;
  let worstState: HardwareItemState = "ok";
  for (const item of items) {
    if (item.required) {
      requiredTotal += 1;
      if (item.state === "ok") requiredOk += 1;
    } else {
      optionalTotal += 1;
      if (item.state === "ok") optionalOk += 1;
    }
    const rank = STATE_RANK[item.state] ?? 0;
    if (item.required && rank > worstRank) {
      worstRank = rank;
      worstState = item.state;
    }
  }
  return { requiredOk, requiredTotal, optionalOk, optionalTotal, worstState };
}
