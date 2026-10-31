// Per-item hardware list. Used by the home-page hardware panel and
// the status tiles. Rendering is deliberately
// compact: one row per item, severity dot, label, detail line, and
// an optional fix-hint chip when the item is not OK. Click a row
// to expand and see the raw item record (id, profile-required, raw
// detail line) for diagnostics.

import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Circle,
  HelpCircle,
  X,
} from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { cn } from "@/lib/utils";
import type { HardwareCheckItem } from "@/shared/agent-api.generated";

interface HardwareItemListProps {
  items: HardwareCheckItem[];
  emptyText?: string;
  showOptionalSection?: boolean;
  className?: string;
}

const STATE_META: Record<
  HardwareCheckItem["state"],
  {
    tone: string;
    Icon: typeof CheckCircle2;
    labelKey:
      | "panels.hardwareList.state.ok"
      | "panels.hardwareList.state.warning"
      | "panels.hardwareList.state.missing"
      | "panels.hardwareList.state.checking"
      | "panels.hardwareList.state.unknown";
  }
> = {
  ok: { tone: "text-ok", Icon: CheckCircle2, labelKey: "panels.hardwareList.state.ok" },
  warning: { tone: "text-warn", Icon: AlertTriangle, labelKey: "panels.hardwareList.state.warning" },
  missing: { tone: "text-destructive", Icon: X, labelKey: "panels.hardwareList.state.missing" },
  checking: { tone: "text-muted-foreground", Icon: Circle, labelKey: "panels.hardwareList.state.checking" },
  unknown: { tone: "text-muted-foreground", Icon: HelpCircle, labelKey: "panels.hardwareList.state.unknown" },
};

export function HardwareItemList({
  items,
  emptyText,
  showOptionalSection = true,
  className,
}: HardwareItemListProps) {
  const { t } = useTranslation();
  if (!items || items.length === 0) {
    return (
      <p className={cn("text-sm text-muted-foreground", className)}>
        {emptyText ?? t("panels.hardwareList.empty")}
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
        <Section title={t("panels.hardwareList.required")} items={required} />
      )}
      {optional.length > 0 && (
        <Section title={t("panels.hardwareList.optional")} items={optional} />
      )}
    </div>
  );
}

function Section({ title, items }: { title: string; items: HardwareCheckItem[] }) {
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

function HardwareItemRow({ item }: { item: HardwareCheckItem }) {
  const { t } = useTranslation();
  const { tone, Icon, labelKey } = STATE_META[item.state];
  const label = t(labelKey);
  const [expanded, setExpanded] = useState(false);
  const Chevron = expanded ? ChevronDown : ChevronRight;

  return (
    <li className="rounded border border-border/60">
      <button
        type="button"
        className="w-full flex items-start gap-2 px-2.5 py-1.5 text-left hover:bg-accent/30 transition-colors rounded"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
      >
        <Chevron
          className="h-3.5 w-3.5 mt-0.5 shrink-0 text-muted-foreground/60"
          aria-hidden
        />
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
              {label}
            </span>
          </div>
          {item.detail && (
            <p className="text-xs text-muted-foreground leading-snug mt-0.5">
              {item.detail}
            </p>
          )}
          {item.fix_hint && item.state !== "ok" && (
            <p className="text-xs text-foreground/80 mt-1">
              <span className="font-medium">{t("panels.hardwareList.fix")}</span> {item.fix_hint}
            </p>
          )}
        </div>
      </button>
      {expanded && (
        <div className="px-2.5 pb-2 pt-1 ml-6 border-t border-border/40 space-y-1 text-xs">
          <Field name="id" value={item.id} mono />
          <Field
            name="required"
            value={item.required ? t("panels.hardwareList.requiredTrue") : t("panels.hardwareList.requiredFalse")}
          />
          <Field name="state" value={item.state} mono />
          {item.detail && <Field name="detail" value={item.detail} />}
          {item.fix_hint && <Field name="fix_hint" value={item.fix_hint} />}
        </div>
      )}
    </li>
  );
}

function Field({
  name,
  value,
  mono,
}: {
  name: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="grid grid-cols-[80px_1fr] gap-2">
      <span className="text-muted-foreground">{name}</span>
      <span className={cn("break-words", mono && "font-mono")}>{value}</span>
    </div>
  );
}
