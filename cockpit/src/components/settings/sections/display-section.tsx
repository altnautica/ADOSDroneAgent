// Display: the kiosk display overlays this board supports and the active one
// (`GET /api/v1/setup/display/options`, residual Python). Read-only:
// installing an overlay edits the boot config and is an installer job.

import { useCallback } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote, Row, StaleBadge } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import {
  DISPLAY_OPTIONS_PATH,
  DISPLAY_POLL_MS,
  activeDisplayId,
  displayRows,
  type DisplayOptionsResponse,
} from "@/lib/settings/display";
import { apiFetch } from "@/shared/api-fetch";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";
import { ResidualMissingNote, SectionGroup } from "./section-kit";

export function DisplaySection() {
  const { t } = useTranslation();
  const opts = useResource(
    useCallback(
      (s: AbortSignal) => apiFetch<DisplayOptionsResponse>(DISPLAY_OPTIONS_PATH, { signal: s }),
      [],
    ),
    DISPLAY_POLL_MS,
  );

  if (!opts.ready) return <EmptyNote>{t("settings.display.loading")}</EmptyNote>;
  if (opts.status === 501 && opts.data == null) return <ResidualMissingNote />;
  if (opts.data == null) return <EmptyNote>{opts.error}</EmptyNote>;

  const rows = displayRows(opts.data);
  const active = activeDisplayId(opts.data);

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <p className="px-[0.75rem] text-[0.8rem] text-muted-foreground">
        <Trans i18nKey="settings.display.description" components={{ code: <code className="font-mono" /> }} />
      </p>

      <SectionGroup title={t("settings.display.title")}>
        <Row
          label={t("settings.display.active")}
          value={
            <span className="flex items-center gap-[0.4rem]">
              {active ?? t("settings.display.none")}
              <StaleBadge stale={opts.stale} />
            </span>
          }
          mono
        />
      </SectionGroup>

      <SectionGroup title={t("settings.display.supportedOnBoard", { board: opts.data.board_id })}>
        {rows.length === 0 ? (
          <EmptyNote>{t("settings.display.noOverlays")}</EmptyNote>
        ) : (
          rows.map((d) => (
            <ListItem
              key={d.id}
              headline={<span className="font-mono">{d.id}</span>}
              supporting={[
                d.label,
                d.resolution ? ` · ${d.resolution}` : "",
                d.touchChip ? t("settings.display.touchSuffix", { chip: d.touchChip }) : "",
              ].join("")}
              selected={d.active}
              trailing={
                d.active ? (
                  <span className="text-[0.7rem] uppercase tracking-wide text-ok">
                    {t("settings.display.activeBadge")}
                  </span>
                ) : undefined
              }
            />
          ))
        )}
      </SectionGroup>
    </div>
  );
}
