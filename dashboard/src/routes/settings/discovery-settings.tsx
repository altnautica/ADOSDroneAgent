import { Trans, useTranslation } from "react-i18next";

import { ConfigToggle, ReadRow } from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";
import { useStatus } from "@/hooks/use-status";
import type { SetupAccessUrl } from "@/shared/agent-api.generated";

/** A single advertised reach entry: its label, source, and clickable URL. */
function ReachRow({ entry }: { entry: SetupAccessUrl }) {
  return (
    <div className="flex items-baseline justify-between gap-3 py-0.5">
      <span className="text-[11px] text-muted-foreground shrink-0">
        {entry.label}
      </span>
      <a
        href={entry.url}
        target="_blank"
        rel="noreferrer"
        className="min-w-0 truncate font-mono text-xs text-info hover:underline"
        title={entry.url}
      >
        {entry.url}
      </a>
    </div>
  );
}

export function DiscoverySettings() {
  const { t } = useTranslation();
  const config = useConfig();
  const status = useStatus();

  const discovery = config.data?.discovery;
  const net = status.data?.network;
  const mdnsHost = net?.mdns_host?.trim() || "";
  const hostname = net?.hostname?.trim() || "";
  const lanHost = status.data?.lan_host?.trim() || "";
  const localIps = net?.local_ips ?? [];
  const reach = status.data?.access_urls ?? [];

  return (
    <div className="space-y-6">
      {/* mDNS toggle + read-only service type. */}
      {config.isLoading ? (
        <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.security.readingConfig")}</p>
      ) : config.isError ? (
        <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
          {t("settingsMore.discovery.readError")}
        </div>
      ) : discovery ? (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-4">
            <ConfigToggle
              configKey="discovery.mdns_enabled"
              label={t("settingsMore.discovery.mdnsLabel")}
              hint={t("settingsMore.discovery.mdnsHint")}
              value={discovery.mdns_enabled}
            />
            {discovery.service_type && (
              <div className="border-t border-border pt-4">
                <ReadRow
                  label={t("settingsMore.discovery.serviceType")}
                  value={discovery.service_type}
                />
              </div>
            )}
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            {t("settingsMore.discovery.unavailable")}
          </CardContent>
        </Card>
      )}

      {/* Reach names — only what the agent advertises. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="text-sm font-semibold">{t("settingsMore.discovery.reachTitle")}</div>

          {status.isError ? (
            <p className="text-[11px] text-destructive">
              {t("settingsMore.discovery.reachReadError")}
            </p>
          ) : (
            <div className="space-y-2">
              {/* mDNS name — the avahi-published hostname, or an honest absence. */}
              <div className="flex items-center justify-between gap-3">
                <span className="text-[11px] text-muted-foreground">
                  {t("settingsMore.discovery.mdnsName")}
                </span>
                {mdnsHost ? (
                  <span className="font-mono text-xs">{mdnsHost}</span>
                ) : (
                  <Badge variant="default" className="font-normal">
                    {t("settingsMore.discovery.nonePublished")}
                  </Badge>
                )}
              </div>
              {!mdnsHost && (
                <p className="text-[11px] text-muted-foreground/80">
                  {t("settingsMore.discovery.noMdnsHint")}
                </p>
              )}

              {hostname && hostname !== mdnsHost && (
                <ReadRow label={t("settingsMore.discovery.hostname")} value={hostname} />
              )}
              {lanHost && lanHost !== mdnsHost && (
                <ReadRow label={t("settingsMore.discovery.lanHost")} value={lanHost} />
              )}
              {localIps.length > 0 && (
                <ReadRow label={t("settingsMore.discovery.lanIps")} value={localIps.join(", ")} />
              )}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Advertised URLs — verbatim, since the agent only lists reachable ones. */}
      {reach.length > 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-2">
            <div className="text-sm font-semibold mb-1">{t("settingsMore.discovery.advertisedLinks")}</div>
            <div className="divide-y divide-border/60">
              {reach.map((entry, idx) => (
                <ReachRow
                  key={`${entry.kind}-${entry.id || entry.url}-${idx}`}
                  entry={entry}
                />
              ))}
            </div>
            <p className="text-[11px] text-muted-foreground pt-1">
              <Trans
                i18nKey="settingsMore.discovery.advertisedHint"
                components={{ code: <span className="font-mono" /> }}
              />
            </p>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
