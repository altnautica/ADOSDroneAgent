import { useQuery } from "@tanstack/react-query";
import { Monitor } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";

import { RiskBadge } from "@/components/settings/risk-badge";
import { Card, CardContent } from "@/components/ui/card";
import { apiFetch } from "@/shared/api-fetch";

interface DisplayOption {
  id: string;
  label: string;
  controller?: string;
  touch_chip?: string;
  resolution?: string;
}

interface DisplayOptionsResponse {
  board_id: string;
  current?: { display_id?: string };
  supported: DisplayOption[];
}

export function DisplaySettings() {
  const { t } = useTranslation();
  const opts = useQuery<DisplayOptionsResponse>({
    queryKey: ["display-options"],
    queryFn: () => apiFetch<DisplayOptionsResponse>("/api/v1/setup/display/options"),
    staleTime: 60_000,
  });

  const current = opts.data?.current?.display_id ?? t("settings.display.none");

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-center gap-2 text-sm font-semibold">
            <Monitor className="h-4 w-4" />
            {t("settings.display.title")}
            <RiskBadge tone="manual" />
          </div>
          <p className="text-xs text-muted-foreground">
            <Trans
              i18nKey="settings.display.description"
              components={{ code: <code className="text-xs" /> }}
            />
          </p>

          <div className="rounded-md border border-border bg-muted/40 px-3 py-2 text-sm">
            <div className="text-xs text-muted-foreground uppercase tracking-wider">
              {t("settings.display.active")}
            </div>
            <div className="font-mono mt-0.5">
              {opts.isLoading ? t("settings.display.loading") : current}
            </div>
          </div>

          {opts.data && opts.data.supported.length > 0 && (
            <div>
              <div className="text-xs text-muted-foreground uppercase tracking-wider mb-2">
                {t("settings.display.supportedOnBoard", { board: opts.data.board_id })}
              </div>
              <ul className="space-y-1.5">
                {opts.data.supported.map((d) => (
                  <li
                    key={d.id}
                    className="text-xs flex items-center gap-2 px-2 py-1 rounded-md border border-border"
                  >
                    <span className="font-mono text-foreground">{d.id}</span>
                    {d.id === current && (
                      <span className="text-[9px] uppercase tracking-wider text-ok">
                        {t("settings.display.activeBadge")}
                      </span>
                    )}
                    <span className="text-muted-foreground truncate">
                      {d.label}
                      {d.resolution && ` · ${d.resolution}`}
                      {d.touch_chip && t("settings.display.touchSuffix", { chip: d.touch_chip })}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}

          {opts.data && opts.data.supported.length === 0 && (
            <p className="text-xs text-muted-foreground">
              {t("settings.display.noOverlays")}
            </p>
          )}

        </CardContent>
      </Card>
    </div>
  );
}
