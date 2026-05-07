import { cn } from "@/lib/utils";

interface Props {
  tone: "auto" | "manual";
  className?: string;
}

export function RiskBadge({ tone, className }: Props) {
  const isAuto = tone === "auto";
  return (
    <span
      title={
        isAuto
          ? "Saved automatically when this field loses focus."
          : "Requires explicit Save and confirmation. Risky for the live agent."
      }
      className={cn(
        "inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider font-medium border",
        isAuto
          ? "border-emerald-500/40 text-emerald-600 dark:text-emerald-400"
          : "border-amber-500/40 text-amber-600 dark:text-amber-400",
        className,
      )}
    >
      <span
        className={cn(
          "inline-block h-1.5 w-1.5 rounded-full",
          isAuto ? "bg-emerald-500" : "bg-amber-500",
        )}
      />
      {isAuto ? "auto" : "manual"}
    </span>
  );
}
