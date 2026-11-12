import { ArrowRight, Fullscreen } from "lucide-react";
import { useTranslation } from "react-i18next";

import { enterCockpit } from "@/lib/cockpit-launch";

// Launch row for the cockpit at /cockpit, the same surface the attached HDMI
// panel renders.
export function CockpitLauncher() {
  const { t } = useTranslation();

  return (
    <button
      type="button"
      onClick={enterCockpit}
      aria-label={t("panels.cockpit.ariaLabel")}
      className="group w-full rounded-md border border-border bg-card px-4 py-3 text-left transition-colors hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
    >
      <div className="flex items-center gap-3">
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md border border-border bg-muted text-muted-foreground">
          <Fullscreen className="h-4 w-4" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5 text-sm font-semibold text-foreground">
            {t("panels.cockpit.enter")}
            <ArrowRight className="h-4 w-4 text-muted-foreground transition-transform group-hover:translate-x-0.5" />
          </div>
          <p className="mt-0.5 text-sm text-muted-foreground">{t("panels.cockpit.description")}</p>
        </div>
        <span className="hidden h-6 shrink-0 items-center rounded-[4px] border border-border px-2 text-xs font-medium text-foreground sm:inline-flex">
          {t("panels.cockpit.launch")}
        </span>
      </div>
    </button>
  );
}
