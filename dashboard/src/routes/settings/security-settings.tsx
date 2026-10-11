import { useQuery } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { ConfigToggle } from "@/components/settings/config-fields";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { useConfig } from "@/hooks/use-config";
import { putConfigChecked } from "@/lib/apply-actions";
import { fetchPinStatus } from "@/lib/pin";
import { toast, toastFromError } from "@/lib/toast";

/** The GET /api/config redaction sentinel: a non-empty api_key comes back as
 * this. We only ever read the set/unset state, never the value. */
const REDACTED = "***";

/** Read-only state row with a coloured badge. */
function StateCard({
  title,
  children,
  badge,
  badgeVariant,
}: {
  title: string;
  children: ReactNode;
  badge: string;
  badgeVariant: "ok" | "warn" | "default";
}) {
  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-2">
        <div className="flex items-center justify-between gap-3">
          <span className="text-sm font-semibold">{title}</span>
          <Badge variant={badgeVariant} className="font-normal">
            {badge}
          </Badge>
        </div>
        <div className="text-xs text-muted-foreground leading-relaxed">
          {children}
        </div>
      </CardContent>
    </Card>
  );
}

/** MAVLink WS auth enforcement: enabling it can lock out an off-box GCS that
 * presents no pairing key, so the enable path is gated behind a confirm. */
function WsEnforceToggle({ value }: { value: boolean | undefined }) {
  const { t } = useTranslation();
  const config = useConfig();
  const [checked, setChecked] = useState(value ?? false);
  const [pendingEnable, setPendingEnable] = useState(false);

  useEffect(() => {
    setChecked(value ?? false);
  }, [value]);

  async function commit(next: boolean) {
    const previous = checked;
    setChecked(next);
    try {
      await putConfigChecked("mavlink.ws_proxy_enforce_auth", String(next));
      toast.ok(next ? t("settingsMore.security.wsEnforceOn") : t("settingsMore.security.wsEnforceOff"));
      config.refetch();
    } catch (err) {
      setChecked(previous);
      toastFromError(err, t("settingsMore.security.wsEnforceError"));
    }
  }

  function onToggle(next: boolean) {
    if (next) {
      setPendingEnable(true);
      return;
    }
    void commit(false);
  }

  return (
    <>
      <div className="flex items-start justify-between gap-4">
        <div className="space-y-1 min-w-0">
          <div className="text-sm font-medium">{t("settingsMore.security.wsEnforceLabel")}</div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            {t("settingsMore.security.wsEnforceHint")}
          </p>
        </div>
        <Switch
          checked={checked}
          onCheckedChange={onToggle}
          aria-label={t("settingsMore.security.wsEnforceLabel")}
        />
      </div>
      <ConfirmDialog
        open={pendingEnable}
        onOpenChange={(open) => {
          if (!open) setPendingEnable(false);
        }}
        title={t("settingsMore.security.wsEnforceConfirmTitle")}
        description={<>{t("settingsMore.security.wsEnforceConfirmBody")}</>}
        confirmLabel={t("settingsMore.security.wsEnforceConfirm")}
        destructive
        onConfirm={() => {
          setPendingEnable(false);
          return commit(true);
        }}
      />
    </>
  );
}

export function SecuritySettings() {
  const { t } = useTranslation();
  const config = useConfig();
  const pin = useQuery({
    queryKey: ["dashboard-pin-status"],
    queryFn: ({ signal }) => fetchPinStatus(signal),
    refetchInterval: 15_000,
  });

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.security.readingConfig")}</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settingsMore.security.readError")}
      </div>
    );
  }

  const security = config.data?.security;
  const apiKeySet = security?.api?.api_key === REDACTED;

  return (
    <div className="space-y-6">
      {/* API key — read-only state, never the value. */}
      <StateCard
        title={t("settingsMore.security.apiKeyTitle")}
        badge={apiKeySet ? t("settingsMore.security.badgeSet") : t("settingsMore.security.badgeNotSet")}
        badgeVariant={apiKeySet ? "ok" : "default"}
      >
        {apiKeySet ? (
          <>{t("settingsMore.security.apiKeySet")}</>
        ) : (
          <>{t("settingsMore.security.apiKeyNotSet")}</>
        )}
      </StateCard>

      {/* Dashboard PIN — read-only; management lives on the access gate. */}
      <StateCard
        title={t("settingsMore.security.pinTitle")}
        badge={
          pin.isLoading
            ? "…"
            : pin.isError
              ? t("settingsMore.security.badgeUnknown")
              : pin.data?.locked
                ? t("settingsMore.security.badgeLocked")
                : pin.data?.pin_set
                  ? t("settingsMore.security.badgeSet")
                  : t("settingsMore.security.badgeNotSet")
        }
        badgeVariant={
          pin.data?.locked
            ? "warn"
            : pin.data?.pin_set
              ? "ok"
              : "default"
        }
      >
        {pin.isError ? (
          <>{t("settingsMore.security.pinReadError")}</>
        ) : pin.data?.pin_set ? (
          <>{t("settingsMore.security.pinSet")}</>
        ) : (
          <>{t("settingsMore.security.pinNotSet")}</>
        )}
      </StateCard>

      {/* Auth enforcement toggles. */}
      {security ? (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <WsEnforceToggle value={config.data?.mavlink?.ws_proxy_enforce_auth} />
            <div className="border-t border-border pt-5">
              <ConfigToggle
                configKey="security.hmac_enabled"
                label={t("settingsMore.security.hmacLabel")}
                hint={t("settingsMore.security.hmacHint")}
                value={security.hmac_enabled}
              />
            </div>
            <div className="border-t border-border pt-5">
              <ConfigToggle
                configKey="security.setup_token_required"
                label={t("settingsMore.security.setupTokenLabel")}
                hint={t("settingsMore.security.setupTokenHint")}
                value={security.setup_token_required}
              />
            </div>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            {t("settingsMore.security.flagsUnavailable")}
          </CardContent>
        </Card>
      )}
    </div>
  );
}
