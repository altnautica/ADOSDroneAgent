import { useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { failureText, installStatusLabel, installStatusVariant } from "@/lib/plugin-install-view";

export interface InstallEntry {
  plugin_id: string;
  version?: string;
  status?: string;
  signer_id?: string | null;
  failure_reason?: string | null;
  permissions?: Record<string, { granted: boolean }>;
}

/** One installed extension: name, status words, why it stopped, and its
 *  Enable/Disable and Remove actions. */
export function InstalledRow({
  install: p,
  name,
  onToggle,
  onRemove,
}: {
  install: InstallEntry;
  name: string;
  onToggle: (action: "enable" | "disable") => void;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  const enabled = p.status === "enabled" || p.status === "running";
  const incompatible = p.status === "incompatible";
  const perms = p.permissions ? Object.values(p.permissions) : [];
  const granted = perms.filter((g) => g.granted).length;
  const failure = failureText(p.failure_reason, t);

  return (
    <Card>
      <CardContent className="pt-4 pb-4 flex flex-wrap items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="text-sm font-medium">{name}</span>
            {p.version && <span className="text-xs text-muted-foreground font-mono">{`v${p.version}`}</span>}
            <Badge variant={installStatusVariant(p.status)} data-status={p.status}>
              {installStatusLabel(p.status, t)}
            </Badge>
            {p.signer_id ? (
              <span className="text-xs text-muted-foreground">{t("extensions.status.signed")}</span>
            ) : (
              <span className="text-xs text-destructive">{t("extensions.status.unsigned")}</span>
            )}
          </div>
          {name !== p.plugin_id && <p className="font-mono text-xs text-muted-foreground mt-0.5">{p.plugin_id}</p>}
          {failure && (
            <p role="status" className="text-xs text-destructive mt-1">
              {failure}
            </p>
          )}
          {incompatible && <p className="text-xs text-warn mt-1">{t("extensions.incompatible")}</p>}
          {perms.length > 0 && (
            <p className="text-xs text-muted-foreground mt-1">
              {t("extensions.status.permissionsGranted", { granted, total: perms.length })}
            </p>
          )}
        </div>
        <div className="shrink-0 flex items-center gap-2">
          <Button
            size="sm"
            variant={enabled ? "outline" : "default"}
            disabled={incompatible}
            onClick={() => onToggle(enabled ? "disable" : "enable")}
          >
            {enabled ? t("extensions.actions.disable") : t("extensions.actions.enable")}
          </Button>
          <Button size="sm" variant="outline" onClick={onRemove}>
            {t("cockpit.extensions.remove")}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
