// The hardware check on the System screen: every probed item from
// `GET /api/v1/setup/status` (required first), its state word and the fix hint
// for anything not ok, plus a Rescan (`POST /api/v1/setup/hardware-check/refresh`,
// residual Python; a 501 is reported as a toast, not hidden). Self-polling.

import { useCallback, useState } from "react";
import { RefreshCw } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ActionButton, EmptyNote, SectionHeader } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { toneClass, type Tone } from "@/lib/tone";
import { apiFetch } from "@/shared/api-fetch";
import type { HardwareCheckItem, SetupStatus } from "@/shared/agent-api.generated";
import { refreshHardwareCheck } from "@/shared/api/setup-actions";
import { toast, toastFromError } from "@/shared/api/toast";
import { fmtRelativeTime } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const STALE_AFTER_MS = 5 * 60 * 1000;

const STATE_TONE: Record<HardwareCheckItem["state"], Tone> = {
  ok: "ok",
  warning: "warn",
  missing: "err",
  checking: "muted",
  unknown: "muted",
};

export function HardwareCard() {
  const { t } = useTranslation();
  // The read time travels with the reading, so the stale check stays pure.
  const setup = useResource(
    useCallback(
      (s: AbortSignal) =>
        apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }).then((status) => ({ status, readAt: Date.now() })),
      [],
    ),
    20_000,
  );
  const [busy, setBusy] = useState(false);

  const check = setup.data?.status.hardware_check;
  const items = [...(check?.items ?? [])].sort((a, b) => Number(b.required) - Number(a.required));
  const lastRun = check?.last_run ? Date.parse(check.last_run) : null;
  const stale = lastRun != null && setup.data != null && setup.data.readAt - lastRun > STALE_AFTER_MS;

  const rescan = () => {
    if (busy) return;
    setBusy(true);
    refreshHardwareCheck()
      .then(() => toast.ok(t("panels.hardware.rescanned")))
      .catch((err: unknown) => toastFromError(err, t("panels.hardware.rescanFailed")))
      .finally(() => {
        setBusy(false);
        setup.refresh();
      });
  };

  return (
    <div className="flex flex-col gap-[0.2rem]">
      <div className="flex items-center justify-between">
        <SectionHeader>{t("panels.hardware.title")}</SectionHeader>
        {stale ? (
          <span className="text-[0.75rem] uppercase text-warn">{t("panels.hardware.stale")}</span>
        ) : lastRun != null ? (
          <span className="font-mono text-[0.75rem] text-muted-foreground">{fmtRelativeTime(lastRun)}</span>
        ) : null}
      </div>
      {items.length === 0 ? (
        <EmptyNote>{t("panels.hardware.scanning")}</EmptyNote>
      ) : (
        items.map((item) => (
          <ListItem
            key={item.id}
            headline={item.label}
            supporting={
              item.state !== "ok" && item.fix_hint
                ? `${t("panels.hardwareList.fix")} ${item.fix_hint}`
                : item.detail || (item.required ? t("panels.hardwareList.required") : t("panels.hardwareList.optional"))
            }
            trailing={
              <span className={`text-[0.8rem] ${toneClass(STATE_TONE[item.state] ?? "muted")}`}>
                {t(`panels.hardwareList.state.${item.state}`)}
              </span>
            }
          />
        ))
      )}
      <div className="mt-[0.3rem]">
        <ActionButton label={t("panels.hardware.rescan")} icon={RefreshCw} onClick={rescan} busy={busy} full />
      </div>
    </div>
  );
}
