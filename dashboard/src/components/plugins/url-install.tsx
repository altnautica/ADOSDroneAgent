import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { isInstallUrl, isSha256OrEmpty } from "@/lib/plugin-install-view";
import type { PluginSource } from "@/shared/api/plugin-install";

/** Install from an archive URL the agent fetches itself. The preview and the
 *  permission review are the same as for an uploaded file. */
export function UrlInstall({ busy, onInstall }: { busy: boolean; onInstall: (source: PluginSource) => void }) {
  const { t } = useTranslation();
  const [url, setUrl] = useState("");
  const [sha, setSha] = useState("");
  const shaOk = isSha256OrEmpty(sha);
  const ready = isInstallUrl(url) && shaOk && !busy;

  return (
    <Card>
      <CardContent className="space-y-3 pt-5 pb-5">
        <div className="text-sm font-medium">{t("extensions.page.installUrl")}</div>
        <div className="space-y-1">
          <Label htmlFor="ext-url">{t("cockpit.extensions.url")}</Label>
          <Input
            id="ext-url"
            value={url}
            placeholder={t("cockpit.extensions.urlPlaceholder")}
            onChange={(e) => setUrl(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor="ext-sha">{t("cockpit.extensions.sha256")}</Label>
          <Input id="ext-sha" value={sha} onChange={(e) => setSha(e.target.value)} className="font-mono" />
          <p className={shaOk ? "text-xs text-muted-foreground" : "text-xs text-destructive"}>
            {shaOk ? t("cockpit.extensions.sha256Hint") : t("cockpit.extensions.sha256Invalid")}
          </p>
        </div>
        <Button
          size="sm"
          disabled={!ready}
          onClick={() => onInstall({ kind: "url", url: url.trim(), sha256: sha.trim() || null })}
        >
          {t("cockpit.extensions.fetchUrl")}
        </Button>
      </CardContent>
    </Card>
  );
}
