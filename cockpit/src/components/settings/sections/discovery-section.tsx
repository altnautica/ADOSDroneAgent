// Discovery: the mDNS advertisement switch and how this node can be reached,
// read from `GET /api/v1/setup/status` and shown only as the agent advertises
// it. Turning mDNS off can withdraw the name the operator is using, so it
// waits behind a guarded button; turning it on writes at once.

import { useCallback } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote, Row, StaleBadge } from "@/components/ui/data";
import { SwitchSettingRow } from "@/components/ui/setting-row";
import { MDNS_KEY, SETUP_STATUS_POLL_MS, mdnsNeedsGuard, reachFacts, reachKey } from "@/lib/settings/discovery";
import type { SetupStatus } from "@/shared/agent-api.generated";
import { apiFetch } from "@/shared/api-fetch";
import { useResource } from "@/shared/use-resource";
import type { AgentConfig } from "@/shared/api/types";

import { PendingLeafBar, SectionGroup } from "./section-kit";
import { useAgentConfig, useGuardedLeaf } from "./section-hooks";

export function DiscoverySection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const leaf = useGuardedLeaf();
  const status = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    SETUP_STATUS_POLL_MS,
  );
  const reach = reachFacts(status.data);
  const discovery = get<AgentConfig["discovery"] | null>("discovery", null);
  const pending = leaf.pending?.key === MDNS_KEY;

  let mdnsBlock;
  if (!ready) mdnsBlock = <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  else if (config == null) mdnsBlock = <EmptyNote>{t("settingsMore.discovery.readError")}</EmptyNote>;
  else if (!discovery) mdnsBlock = <EmptyNote>{t("settingsMore.discovery.unavailable")}</EmptyNote>;
  else
    mdnsBlock = (
      <>
        <SwitchSettingRow
          title={t("settingsMore.discovery.mdnsLabel")}
          summary={t("settingsMore.discovery.mdnsHint")}
          checked={pending ? false : discovery.mdns_enabled === true}
          enabled={!pending}
          onChange={(next) => void leaf.request({ key: MDNS_KEY, value: next }, mdnsNeedsGuard(next))}
        />
        {pending ? (
          <PendingLeafBar
            prompt={t("cockpit.settingsSections.discovery.mdnsPrompt")}
            fireLabel={t("cockpit.settingsSections.discovery.mdnsFire")}
            holdLabel={t("cockpit.settingsSections.discovery.mdnsHold")}
            onFire={leaf.fire}
            onCancel={leaf.cancel}
            disabled={leaf.busy}
          />
        ) : null}
        {discovery.service_type ? (
          <Row label={t("settingsMore.discovery.serviceType")} value={discovery.service_type} />
        ) : null}
      </>
    );

  return (
    <div className="flex flex-col gap-[0.8rem]">
      <SectionGroup>{mdnsBlock}</SectionGroup>

      <SectionGroup
        title={
          <span className="flex items-center justify-between">
            {t("settingsMore.discovery.reachTitle")}
            <StaleBadge stale={status.stale && status.data != null} />
          </span>
        }
      >
        {status.data == null && status.ready ? (
          <EmptyNote>{t("settingsMore.discovery.reachReadError")}</EmptyNote>
        ) : status.data == null ? (
          <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>
        ) : (
          <>
            <Row
              label={t("settingsMore.discovery.mdnsName")}
              value={reach.mdnsHost || t("settingsMore.discovery.nonePublished")}
              tone={reach.mdnsHost ? undefined : "muted"}
              hint={reach.mdnsHost ? undefined : t("settingsMore.discovery.noMdnsHint")}
            />
            {reach.hostname ? <Row label={t("settingsMore.discovery.hostname")} value={reach.hostname} /> : null}
            {reach.lanHost ? <Row label={t("settingsMore.discovery.lanHost")} value={reach.lanHost} /> : null}
            {reach.localIps.length > 0 ? (
              <Row label={t("settingsMore.discovery.lanIps")} value={reach.localIps.join(", ")} />
            ) : null}
          </>
        )}
      </SectionGroup>

      {reach.urls.length > 0 ? (
        <SectionGroup title={t("settingsMore.discovery.advertisedLinks")}>
          {reach.urls.map((entry, idx) => (
            <Row key={reachKey(entry, idx)} label={entry.label} value={entry.url} />
          ))}
          <p className="px-[0.75rem] text-[0.75rem] text-muted-foreground">
            <Trans i18nKey="settingsMore.discovery.advertisedHint" components={{ code: <span className="font-mono" /> }} />
          </p>
        </SectionGroup>
      ) : null}
    </div>
  );
}
