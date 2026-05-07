// Risk badge for the plugin install dialog. 4 levels with shape +
// color + icon (color alone is never the signal, color-blind safe).
// Spec: 17-ux-install-and-permissions section 3.

import { AlertTriangle, Check, Circle, ShieldAlert } from "lucide-react";

import type { RiskLevel } from "@/lib/plugin-install";
import { cn } from "@/lib/utils";

interface PluginRiskBadgeProps {
  level: RiskLevel;
  size?: "sm" | "md";
  showLabel?: boolean;
  className?: string;
}

const META: Record<
  RiskLevel,
  { label: string; tone: string; Icon: typeof Check }
> = {
  low: {
    label: "Low risk",
    tone: "border-ok/40 text-ok bg-ok/10",
    Icon: Check,
  },
  medium: {
    label: "Medium risk",
    tone: "border-warn/40 text-warn bg-warn/10",
    Icon: Circle,
  },
  high: {
    label: "High risk",
    tone: "border-warn/60 text-warn bg-warn/15",
    Icon: AlertTriangle,
  },
  critical: {
    label: "Critical risk",
    tone: "border-destructive/60 text-destructive bg-destructive/15",
    Icon: ShieldAlert,
  },
};

export function PluginRiskBadge({
  level,
  size = "md",
  showLabel = true,
  className,
}: PluginRiskBadgeProps) {
  const { label, tone, Icon } = META[level];
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
      {showLabel && <span>{label.replace(" risk", "")}</span>}
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
  const { label, Icon } = META[level];
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
