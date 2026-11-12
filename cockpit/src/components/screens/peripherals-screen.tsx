// Peripherals: every registered peripheral manifest with its live connection
// state (`GET /api/v1/peripherals`, a residual route that answers 501 when the
// residual service is absent). Read-only.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { Dot, EmptyNote, ErrorNote, StaleBadge } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { parsePeripherals, sinceParts } from "@/lib/peripherals";
import { peripheralStateKey } from "@/lib/screen-words";
import { useNavStore } from "@/stores/nav-store";
import { toneClass } from "@/lib/tone";
import { apiFetch } from "@/shared/api-fetch";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const POLL_MS = 8000;

export function PeripheralsScreen() {
  const { t } = useTranslation();
  const list = useResource(
    useCallback((s: AbortSignal) => apiFetch<unknown>("/api/v1/peripherals", { signal: s }).then(parsePeripherals), []),
    POLL_MS,
  );
  const items = list.data ?? [];

  let body;
  if (list.status === 501 && !list.data) body = <EmptyNote>{t("cockpit.peripherals.residualAbsent")}</EmptyNote>;
  else if (!list.ready) body = <EmptyNote>{t("peripherals.page.loading")}</EmptyNote>;
  else if (list.error && !list.data) body = <ErrorNote onRetry={list.refresh}>{t("cockpit.peripherals.unavailable")}</ErrorNote>;
  else if (items.length === 0) body = <EmptyNote>{t("peripherals.empty.title")}</EmptyNote>;
  else {
    body = (
      <div className="flex flex-col gap-[0.2rem]">
        <p className="mb-[0.3rem] text-[0.8rem] text-muted-foreground">{t("peripherals.page.blurb")}</p>
        {items.map((p) => {
          const stateKey = peripheralStateKey(p);
          const stateText = stateKey ? t(stateKey) : p.state;
          const since = p.lastSeen !== null ? sinceParts(p.lastSeen) : null;
          return (
            <ListItem
              key={p.id}
              headline={
                <span className="flex items-center gap-[0.4rem]">
                  <Dot tone={p.tone} />
                  {p.name}
                </span>
              }
              supporting={[
                ...p.detail,
                p.description,
                since
                  ? t("peripherals.page.lastSeen", { time: t(`peripherals.time.${since.unit}`, { n: since.n }) })
                  : null,
              ]
                .filter(Boolean)
                .join(" · ")}
              trailing={<span className={`text-[0.75rem] uppercase tracking-wider ${toneClass(p.tone)}`}>{stateText}</span>}
            />
          );
        })}
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={list.stale}
      title={t("peripherals.page.title")}
      onBack={() => useNavStore.getState().back()}
      actions={<StaleBadge stale={list.stale} />}
    >
      {body}
    </ConsoleScaffold>
  );
}
