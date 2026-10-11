// Risk badge for the plugin install dialog. 4 levels with shape +
// color + icon (color alone is never the signal, color-blind safe).
// Spec: 17-ux-install-and-permissions section 3.

import { AlertTriangle, Check, Circle, ShieldAlert } from "lucide-react";
import { useTranslation } from "react-i18next";

import type { RiskLevel } from "@/lib/plugin-install";
import { cn } from "@/lib/utils";

interface PluginRiskBadgeProps {
  level: RiskLevel;
  size?: "sm" | "md";
  showLabel?: boolean;
  className?: string;
}

const META = {
  low: {
    labelKey: "extensions.risk.low",
    shortKey: "extensions.risk.lowShort",
    tone: "border-ok/40 text-ok bg-ok/10",
    Icon: Check,
  },
  medium: {
    labelKey: "extensions.risk.medium",
    shortKey: "extensions.risk.mediumShort",
    tone: "border-warn/40 text-warn bg-warn/10",
    Icon: Circle,
  },
  high: {
    labelKey: "extensions.risk.high",
    shortKey: "extensions.risk.highShort",
    tone: "border-warn/60 text-warn bg-warn/15",
    Icon: AlertTriangle,
  },
  critical: {
    labelKey: "extensions.risk.critical",
    shortKey: "extensions.risk.criticalShort",
    tone: "border-destructive/60 text-destructive bg-destructive/15",
    Icon: ShieldAlert,
  },
} as const satisfies Record<RiskLevel, unknown>;

export function PluginRiskBadge({
  level,
  size = "md",
  showLabel = true,
  className,
}: PluginRiskBadgeProps) {
  const { t } = useTranslation();
  const { labelKey, shortKey, tone, Icon } = META[level];
  const label = t(labelKey);
  const padding = size === "sm" ? "px-1.5 py-0.5" : "px-2 py-0.5";
  const text = size === "sm" ? "text-[10px]" : "text-xs";
  const icon = size === "sm" ? "h-3 w-3" : "h-3.5 w-3.5";
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className={cn(
        "inline-flex items-center gap-1 rounded border font-medium uppercase tracking-wider",
        padding,
        text,
        tone,
        className,
      )}
    >
      <Icon className={icon} aria-hidden />
      {showLabel && <span>{t(shortKey)}</span>}
    </span>
  );
}

interface RiskDotProps {
  level: RiskLevel;
  className?: string;
}

// Compact, per-row dot for the permission table. Same color/shape
// taxonomy as the badge but icon-only with a screen-reader label.
export function RiskDot({ level, className }: RiskDotProps) {
  const { t } = useTranslation();
  const { labelKey, Icon } = META[level];
  const label = t(labelKey);
  const tone =
    level === "low"
      ? "text-ok"
      : level === "medium"
        ? "text-warn/80"
        : level === "high"
          ? "text-warn"
          : "text-destructive";
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className={cn("inline-flex items-center justify-center", tone, className)}
    >
      <Icon className="h-3.5 w-3.5" aria-hidden />
    </span>
  );
}
