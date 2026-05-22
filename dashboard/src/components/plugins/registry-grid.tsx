import { Check, Download, ExternalLink, Shield } from "lucide-react";
import { useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
import { toast, toastFromError } from "@/lib/toast";

export interface CatalogEntry {
  id: string;
  name: string;
  version: string;
  description: string;
  author?: string;
  homepage?: string;
  license?: string;
  risk?: "low" | "medium" | "high" | "critical";
  category?: string;
  halves?: ("agent" | "gcs")[];
  download_url: string;
  archive_sha256?: string | null;
}

interface CatalogResponse {
  schema_version: number;
  source: string;
  plugins: CatalogEntry[];
  error?: string;
}

interface Props {
  installedIds: Set<string>;
  onInstalled?: () => void;
}

const RISK_TONE: Record<
  NonNullable<CatalogEntry["risk"]>,
  { label: string; classes: string }
> = {
  low: {
    label: "Low risk",
    classes: "border-ok/40 text-ok",
  },
  medium: {
    label: "Medium risk",
    classes: "border-warn/40 text-warn",
  },
  high: {
    label: "High risk",
    classes: "border-destructive/40 text-destructive",
  },
  critical: {
    label: "Critical",
    classes:
      "border-destructive bg-destructive/10 text-destructive font-semibold",
  },
};

export function RegistryGrid({ installedIds, onInstalled }: Props) {
  const catalog = useResource<CatalogResponse>(
    "plugin-catalog",
    "/api/v1/plugins/catalog",
    30000,
  );
  const [installing, setInstalling] = useState<CatalogEntry | null>(null);
  const [pending, setPending] = useState(false);

  const entries = catalog.data?.plugins ?? [];
  if (entries.length === 0) return null;

  async function handleInstall(entry: CatalogEntry) {
    setPending(true);
    try {
      const body: Record<string, unknown> = {
        url: entry.download_url,
        from_catalog: true,
      };
      if (entry.archive_sha256) body.expected_sha256 = entry.archive_sha256;
      await apiFetch("/api/plugins/install_from_url", {
        method: "POST",
        body,
      });
      toast.ok(`${entry.name} installed.`);
      onInstalled?.();
      setInstalling(null);
    } catch (err) {
      toastFromError(err, "Plugin install failed.");
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">Available plugins</h2>
        <span className="text-xs text-muted-foreground">
          First-party catalog. Downloaded fresh each install.
        </span>
      </div>
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-2">
        {entries.map((entry) => {
          const isInstalled = installedIds.has(entry.id);
          const risk = entry.risk ?? "low";
          const tone = RISK_TONE[risk];
          return (
            <Card key={entry.id}>
              <CardContent className="pt-4 pb-4 space-y-3">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2 flex-wrap">
                      <span className="font-medium text-sm">{entry.name}</span>
                      <span className="text-[10px] text-muted-foreground font-mono">
                        v{entry.version}
                      </span>
                      <span
                        className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${tone.classes}`}
                      >
                        {tone.label}
                      </span>
                    </div>
                    <p className="text-xs text-muted-foreground mt-1">
                      {entry.description}
                    </p>
                  </div>
                  <div className="shrink-0">
                    {isInstalled ? (
                      <span className="text-[10px] uppercase tracking-wider text-ok flex items-center gap-1">
                        <Check className="h-3 w-3" />
                        installed
                      </span>
                    ) : (
                      <Button
                        size="sm"
                        onClick={() => setInstalling(entry)}
                        disabled={pending}
                      >
                        <Download className="h-3.5 w-3.5" />
                        Install
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
                      View source
                      <ExternalLink className="h-3 w-3" />
                    </a>
                  )}
                </div>
              </CardContent>
            </Card>
          );
        })}
      </div>

      <ConfirmDialog
        open={!!installing}
        onOpenChange={(open) => {
          if (!open && !pending) setInstalling(null);
        }}
        title={installing ? `Install ${installing.name}?` : ""}
        description={
          installing
            ? `Risk level: ${installing.risk ?? "low"}. The agent will download the archive from the publisher and install it. You will be prompted to grant capabilities the plugin requests.`
            : ""
        }
        confirmLabel={pending ? "Installing…" : "Install"}
        onConfirm={async () => {
          if (installing) await handleInstall(installing);
        }}
      />
    </div>
  );
}
