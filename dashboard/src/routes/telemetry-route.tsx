import { useState } from "react";
import { useTranslation } from "react-i18next";

import { PageShell } from "@/components/page-shell";
import { ParametersTab } from "@/components/telemetry/parameters-tab";
import { SensorsTab } from "@/components/telemetry/sensors-tab";
import { cn } from "@/lib/utils";

export function TelemetryRoute() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<"parameters" | "sensors">("parameters");

  return (
    <PageShell
      title={t("telemetry.page.title")}
      blurb={t("telemetry.page.blurb")}
      maxWidth="max-w-6xl"
      rightAction={
        <div className="flex items-center gap-1 rounded-md border border-border p-0.5 text-xs">
          <button
            type="button"
            onClick={() => setTab("parameters")}
            className={cn(
              "px-3 py-1 rounded transition-colors",
              tab === "parameters"
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:bg-accent/40",
            )}
          >
            {t("telemetry.tabs.parameters")}
          </button>
          <button
            type="button"
            onClick={() => setTab("sensors")}
            className={cn(
              "px-3 py-1 rounded transition-colors",
              tab === "sensors"
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:bg-accent/40",
            )}
          >
            {t("telemetry.tabs.sensors")}
          </button>
        </div>
      }
    >
      {tab === "parameters" ? <ParametersTab /> : <SensorsTab />}
    </PageShell>
  );
}
