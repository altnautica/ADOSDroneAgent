import { useTranslation } from "react-i18next";
import { useSearchParams } from "react-router-dom";

import { PageShell } from "@/components/page-shell";
import { ParametersTab } from "@/components/telemetry/parameters-tab";
import { SensorsTab } from "@/components/telemetry/sensors-tab";
import { telemetryTab, type TelemetryTab } from "@/lib/telemetry-tab";
import { cn } from "@/lib/utils";

export function TelemetryRoute() {
  const { t } = useTranslation();
  // The tab lives in `?tab=`, so a reload or a shared link keeps it.
  const [params, setParams] = useSearchParams();
  const tab = telemetryTab(params.get("tab"));
  const setTab = (next: TelemetryTab) => setParams(next === "parameters" ? {} : { tab: next }, { replace: true });

  return (
    <PageShell
      title={t("telemetry.page.title")}
      blurb={t("telemetry.page.blurb")}
      maxWidth="max-w-6xl"
      rightAction={
        <div role="tablist" className="flex items-center gap-1 rounded-md border border-border p-0.5 text-xs">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "parameters"}
            aria-controls="telemetry-panel"
            onClick={() => setTab("parameters")}
            className={cn(
              "px-3 py-1 rounded transition-colors max-md:min-h-11",
              tab === "parameters"
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:bg-accent/40",
            )}
          >
            {t("telemetry.tabs.parameters")}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "sensors"}
            aria-controls="telemetry-panel"
            onClick={() => setTab("sensors")}
            className={cn(
              "px-3 py-1 rounded transition-colors max-md:min-h-11",
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
      <div id="telemetry-panel" role="tabpanel">
        {tab === "parameters" ? <ParametersTab /> : <SensorsTab />}
      </div>
    </PageShell>
  );
}
