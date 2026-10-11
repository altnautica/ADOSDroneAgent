import { useTranslation } from "react-i18next";

import { cn } from "@/lib/utils";

interface Props {
  tone: "auto" | "manual";
  className?: string;
}

export function RiskBadge({ tone, className }: Props) {
  const { t } = useTranslation();
  const isAuto = tone === "auto";
  return (
    <span
      title={
        isAuto
          ? t("settings.shared.riskAutoTitle")
          : t("settings.shared.riskManualTitle")
      }
      className={cn(
        "inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider font-medium border",
        isAuto
          ? "border-ok/40 text-ok"
          : "border-warn/40 text-warn",
        className,
      )}
    >
      <span
        className={cn(
          "inline-block h-1.5 w-1.5 rounded-full",
          isAuto ? "bg-ok" : "bg-warn",
        )}
      />
      {isAuto ? t("settings.shared.riskAuto") : t("settings.shared.riskManual")}
    </span>
  );
}
