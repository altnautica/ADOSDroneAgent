// The cloud relay on the System screen: the chosen posture, and when the relay
// is on, its broker session, the last status POST and the pairing state.
// `GET /api/cloud/link` answers 404 while the relay is not reporting, which is
// shown as such. Self-polling; the link is not polled in local mode.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { EmptyNote, Row, SectionHeader } from "@/components/ui/data";
import { cloudHeartbeat, type CloudLink } from "@/lib/home-panels";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import type { DashboardSnapshot } from "@/shared/api/types";
import { DASH } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

export function CloudCard() {
  const { t } = useTranslation();
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    20_000,
  );
  const snap = useResource(
    useCallback((s: AbortSignal) => apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal: s }), []),
    5000,
  );
  const mode = setup.data?.cloud_choice?.mode ?? null;
  const isLocal = mode === "local";
  // The read time travels with the reading, so the age stays a pure render.
  const link = useResource(
    useCallback(
      (s: AbortSignal) => apiFetch<CloudLink>("/api/cloud/link", { signal: s }).then((l) => ({ l, at: Date.now() })),
      [],
    ),
    5000,
    { enabled: mode != null && !isLocal },
  );

  const code = snap.data?.cloud?.pairing_code ?? "";
  const finalized = setup.data?.setup_finalized ?? false;

  let linkRows;
  if (!link.data) {
    linkRows = link.ready ? <EmptyNote>{t("panels.cloud.relayNotReporting")}</EmptyNote> : null;
  } else {
    const l = link.data.l;
    const hbLine = cloudHeartbeat(l, link.data.at);
    const heartbeat =
      hbLine.key === "heartbeatOk"
        ? t("panels.cloud.heartbeatOk", { age: hbLine.age })
        : hbLine.key === "heartbeatHttp"
          ? t("panels.cloud.heartbeatHttp", { status: hbLine.status })
          : t(`panels.cloud.${hbLine.key}`);
    linkRows = (
      <>
        <Row
          label={t("panels.cloud.broker")}
          value={l.broker_connected === null ? t("panels.cloud.noSession") : l.broker_connected ? t("panels.cloud.connected") : t("panels.cloud.down")}
          tone={l.broker_connected ? "ok" : "muted"}
        />
        <Row label={t("panels.cloud.statusPost")} value={heartbeat} />
      </>
    );
  }

  return (
    <div className="flex flex-col gap-[0.15rem]">
      <SectionHeader>{t("panels.cloud.title")}</SectionHeader>
      <Row label={t("panels.cloud.mode")} value={mode ?? DASH} />
      {isLocal ? (
        <EmptyNote>{t("panels.cloud.relayDisabled")}</EmptyNote>
      ) : mode == null ? null : (
        <>
          {linkRows}
          <Row
            label={t("panels.cloud.pairing")}
            value={finalized && !code ? t("panels.cloud.paired") : code || t("panels.cloud.awaitingCode")}
            tone={finalized && !code ? "ok" : undefined}
          />
        </>
      )}
    </div>
  );
}
