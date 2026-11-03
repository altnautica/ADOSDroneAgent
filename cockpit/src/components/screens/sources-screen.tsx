// Sources — a receiver ground station's combined FEC output and what each relay
// contributes to it (`GET /api/v1/ground-station/wfb/receiver/combined` and
// `/relays`). Read-only. A receive loop that is not reporting says so: relay
// contributions are then unknown, which is not the same as "no relay".

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { EmptyNote, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { parseReceiverCombined, parseReceiverRelays, relayShares } from "@/lib/ground-station";
import { apiFetch } from "@/shared/api-fetch";
import { DASH, fmtBitrate, fmtInt, fmtRelativeTime } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const POLL_MS = 2000;

export function SourcesScreen() {
  const { t } = useTranslation();
  const combined = useResource(
    useCallback(
      (s: AbortSignal) =>
        apiFetch<unknown>("/api/v1/ground-station/wfb/receiver/combined", { signal: s }).then(parseReceiverCombined),
      [],
    ),
    POLL_MS,
  );
  const relays = useResource(
    useCallback(
      (s: AbortSignal) =>
        apiFetch<unknown>("/api/v1/ground-station/wfb/receiver/relays", { signal: s }).then(parseReceiverRelays),
      [],
    ),
    POLL_MS,
  );

  const c = combined.data;
  const perSource = relays.data?.relays ?? null;
  const notReporting = c?.stale === true;

  let list;
  if (perSource == null) {
    list = relays.ready ? <EmptyNote>{t("panels.sources.notReporting")}</EmptyNote> : null;
  } else if (perSource.length === 0) {
    list = <EmptyNote>{t("panels.sources.noRelayContributing")}</EmptyNote>;
  } else {
    list = relayShares(perSource).map((r) => (
      <ListItem
        key={r.mac}
        headline={<span className="font-mono">{r.mac}</span>}
        supporting={r.lastSeenMs != null ? fmtRelativeTime(r.lastSeenMs) : undefined}
        trailing={
          <span className="font-mono text-[0.85rem] text-surface-foreground">
            {t("panels.sources.frags", { frags: fmtInt(r.fragments) })}
            {r.share != null ? ` · ${Math.round(r.share * 100)}%` : ""}
          </span>
        }
      />
    ));
  }

  return (
    <ConsoleScaffold
      title={t("panels.sources.title")}
      actions={
        <span className="flex items-center gap-[0.5rem]">
          {notReporting ? (
            <span className="text-[0.75rem] uppercase text-warn">{t("panels.sources.stale")}</span>
          ) : null}
          <StaleBadge stale={combined.stale || relays.stale} />
        </span>
      }
    >
      <div className="flex flex-col gap-[0.5rem]">
        {c == null && combined.error ? (
          <EmptyNote>{t("panels.sources.readError")}</EmptyNote>
        ) : (
          <TileGrid>
            <Tile label={t("panels.sources.combinedOutput")} value={fmtBitrate(c?.outputKbps)} />
            <Tile label={t("panels.sources.fragmentsDedup")} value={fmtInt(c?.fragmentsAfterDedup)} />
            <Tile label={t("panels.sources.fecRepaired")} value={fmtInt(c?.fecRepaired)} />
            <Tile label={t("panels.sources.relays")} value={perSource ? String(perSource.length) : DASH} />
          </TileGrid>
        )}
        <div className="flex flex-col gap-[0.2rem]">{list}</div>
      </div>
    </ConsoleScaffold>
  );
}
