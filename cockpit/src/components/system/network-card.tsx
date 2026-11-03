// The node's network on the System screen: uplink kind, its RSSI and every
// interface address, from `GET /api/v1/setup/status`. Self-polling.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { EmptyNote, Row, SectionHeader } from "@/components/ui/data";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import { DASH, fmtDbm } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

export function NetworkCard() {
  const { t } = useTranslation();
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    20_000,
  );
  const net = setup.data?.network;
  const interfaces = Object.entries(net?.ip_addresses ?? {});

  return (
    <div className="flex flex-col gap-[0.15rem]">
      <SectionHeader>{t("panels.network.title")}</SectionHeader>
      <Row label={t("panels.network.uplink")} value={net?.uplink_kind ?? DASH} />
      <Row label={t("panels.network.rssi")} value={fmtDbm(net?.rssi_dbm ?? null)} />
      {interfaces.length > 0 ? (
        interfaces.map(([iface, addr]) => <Row key={iface} label={iface} value={addr || DASH} />)
      ) : (
        <EmptyNote>{t("panels.network.noInterfaces")}</EmptyNote>
      )}
    </div>
  );
}
