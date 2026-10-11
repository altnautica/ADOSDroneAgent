import { AlertCircle, ArrowUpCircle, Check, Download, ExternalLink, Shield } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  type CatalogEntry,
  type CatalogResponse,
  catalogView,
  installState,
} from "@/lib/plugin-catalog";
import { apiFetch } from "@/shared/api-fetch";

interface Props {
  /** Installed plugin id → installed version. */
  installedVersions: ReadonlyMap<string, string | undefined>;
  /** True while an install preview is being prepared. */
  busy: boolean;
  /** Start the two-stage install (manifest preview, then permission approval)
   *  for a catalog entry. Also used to update to a newer catalog version. */
  onInstall: (entry: CatalogEntry) => void;
}

// Risk levels map to the same visual semantics as the Mission Control
// RiskBadge (low=green, medium=blue, high=amber, critical=red), each
// rendered as a border / soft fill / text triple so both surfaces tell
// the operator the same story. The agent uses its own Tailwind token
// names (ok / info / warn / destructive) but the colour intent is
// identical to Mission Control's status-success / accent-primary /
// status-warning / status-error.
const RISK_TONE = {
  low: {
    labelKey: "extensions.risk.low",
    classes: "border-ok/40 bg-ok/10 text-ok",
  },
  medium: {
    labelKey: "extensions.risk.medium",
    classes: "border-info/40 bg-info/10 text-info",
  },
  high: {
    labelKey: "extensions.risk.high",
    classes: "border-warn/40 bg-warn/10 text-warn",
  },
  critical: {
    labelKey: "extensions.risk.critical",
    classes:
      "border-destructive/40 bg-destructive/10 text-destructive font-semibold",
  },
} as const satisfies Record<NonNullable<CatalogEntry["risk"]>, { labelKey: string; classes: string }>;

export function RegistryGrid({ installedVersions, busy, onInstall }: Props) {
  const { t } = useTranslation();
  // The catalog is compiled into the agent, so it cannot change while the page
  // is open: fetch once, never poll.
  const catalog = useQuery<CatalogResponse>({
    queryKey: ["plugin-catalog"],
    queryFn: ({ signal }) => apiFetch<CatalogResponse>("/api/v1/plugins/catalog", { signal }),
    staleTime: Infinity,
  });

  const view = catalogView(catalog.data, catalog.error, catalog.isPending);
  if (view.kind === "loading" || view.kind === "empty") return null;

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">{t("extensions.catalog.title")}</h2>
        <span className="text-xs text-muted-foreground">
          {t("extensions.catalog.subtitle")}
        </span>
      </div>
      {(view.kind === "error" || view.parseError) && (
        <div
          role="alert"
          className="flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
        >
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />
          <span>{view.kind === "error" ? view.message : view.parseError}</span>
          {view.kind === "error" && (
            <Button size="sm" variant="outline" className="ml-auto" onClick={() => void catalog.refetch()}>
              {t("common.retry")}
            </Button>
          )}
        </div>
      )}
      {view.kind === "list" && (
        <div className="grid grid-cols-1 lg:grid-cols-2 gap-2">
          {view.entries.map((entry) => {
            const state = installState(entry, installedVersions);
            // An entry that carries no grade shows none; the install dialog grades
            // every permission from the agent's catalog before anything lands.
            const tone = entry.risk ? RISK_TONE[entry.risk] : null;
            return (
              <Card key={entry.id}>
                <CardContent className="pt-4 pb-4 space-y-3">
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <div className="flex items-center gap-2 flex-wrap">
                        <span className="font-medium text-sm">{entry.name}</span>
                        <span className="text-[10px] text-muted-foreground font-mono">
                          {`v${entry.version}`}
                        </span>
                        {tone && (
                          <span
                            className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${tone.classes}`}
                          >
                            {t(tone.labelKey)}
                          </span>
                        )}
                      </div>
                      <p className="text-xs text-muted-foreground mt-1">
                        {entry.description}
                      </p>
                    </div>
                    <div className="shrink-0">
                      {state === "installed" ? (
                        <span className="text-[10px] uppercase tracking-wider text-ok flex items-center gap-1">
                          <Check className="h-3 w-3" />
                          {t("extensions.catalog.installed")}
                        </span>
                      ) : state === "update" ? (
                        <div className="flex flex-col items-end gap-1">
                          <span className="text-[10px] uppercase tracking-wider text-info">
                            {t("extensions.catalog.updateAvailable")}
                          </span>
                          <Button size="sm" variant="outline" onClick={() => onInstall(entry)} disabled={busy}>
                            <ArrowUpCircle className="h-3.5 w-3.5" />
                            {t("extensions.catalog.updateTo", { version: entry.version })}
                          </Button>
                        </div>
                      ) : (
                        <Button
                          size="sm"
                          onClick={() => onInstall(entry)}
                          disabled={busy}
                        >
                          <Download className="h-3.5 w-3.5" />
                          {t("extensions.page.install")}
                        </Button>
                      )}
                    </div>
                  </div>
                  <div className="flex items-center justify-between text-[11px] text-muted-foreground">
                    <div className="flex items-center gap-3">
                      {entry.author && <span>{entry.author}</span>}
                      {entry.category && (
                        <span className="font-mono">{entry.category}</span>
                      )}
                      {entry.halves && entry.halves.length > 0 && (
                        <span className="flex items-center gap-1">
                          <Shield className="h-3 w-3" />
                          {entry.halves.join(" + ")}
                        </span>
                      )}
                    </div>
                    {entry.homepage && (
                      <a
                        href={entry.homepage}
                        target="_blank"
                        rel="noreferrer"
                        className="hover:underline flex items-center gap-1"
                      >
                        {t("extensions.catalog.viewSource")}
                        <ExternalLink className="h-3 w-3" />
                      </a>
                    )}
                  </div>
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
