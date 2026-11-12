// Capture: the capture storage budget (`/api/v1/storage`) and the exposure the
// world-engine extension's capture service records with. The extension answers
// 404 through the plugin proxy when it is not installed on this node.

import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";

import { EmptyNote, Row, StaleBadge } from "@/components/ui/data";
import { SelectSettingRow, SwitchSettingRow, TextSettingRow } from "@/components/ui/setting-row";
import { getCaptureState } from "@/lib/capture-model";
import {
  CAPTURE_STATE_POLL_MS,
  EXPOSURE_MODES,
  STORAGE_POLL_MS,
  captureStateTone,
  exposureDirty,
  exposureNotApplied,
  exposureRequest,
  exposureSeedKey,
  toDraft,
  type ExposureDraft,
  type ExposureMode,
} from "@/lib/settings/capture";
import { apiFetch } from "@/shared/api-fetch";
import { putCaptureExposure } from "@/shared/api/capture";
import { STORAGE_PATH, type StorageStatus } from "@/shared/api/sensors";
import { toast, toastFromError } from "@/shared/api/toast";
import { fmtNum } from "@/shared/format";
import { useResource } from "@/shared/use-resource";
import "@/shared/i18n";

import { ApplyBar, SectionGroup } from "./section-kit";

function StorageGroup() {
  const { t } = useTranslation();
  const storage = useResource(
    useCallback((s: AbortSignal) => apiFetch<StorageStatus>(STORAGE_PATH, { signal: s }), []),
    STORAGE_POLL_MS,
  );
  const s = storage.data;
  return (
    <SectionGroup title={t("settings.capture.storageTitle")}>
      <div className="flex justify-end px-[0.75rem]">
        <StaleBadge stale={storage.stale} />
      </div>
      {!s ? (
        <EmptyNote>{storage.ready ? t("cockpit.settingsSections.capture.storageFailed") : t("common.loading")}</EmptyNote>
      ) : !s.present ? (
        <EmptyNote>{t("settings.capture.storageAbsent")}</EmptyNote>
      ) : (
        <>
          <Row label={t("settings.capture.root")} value={s.capture_root ?? t("common.dash")} />
          {s.capture ? (
            <>
              <Row
                label={t("settings.capture.budget")}
                value={t("settings.capture.budgetValue", {
                  used: fmtNum(s.capture.used_bytes / 1e9, 1),
                  budget: fmtNum(s.capture.budget_bytes / 1e9, 1),
                  pct: s.capture.budget_pct,
                })}
              />
              <Row
                label={t("settings.capture.sessions")}
                value={t("settings.capture.sessionsValue", { sessions: s.capture.sessions, uploaded: s.capture.uploaded })}
              />
              <Row label={t("settings.capture.state")} value={s.capture.state} tone={captureStateTone(s.capture.state)} />
            </>
          ) : null}
          {s.write_probe ? (
            <Row label={t("settings.capture.writeProbe")} value={t("settings.capture.mbps", { mbps: fmtNum(s.write_probe.mbps, 0) })} />
          ) : null}
        </>
      )}
    </SectionGroup>
  );
}

function ExposureGroup() {
  const { t } = useTranslation();
  const capture = useResource(useCallback((s: AbortSignal) => getCaptureState(s), []), CAPTURE_STATE_POLL_MS);
  const current = capture.data?.exposure ?? null;
  // The plugin proxy answers 404 when the world-engine extension is absent.
  const missing = capture.status === 404;

  // The draft re-seeds when the agent's values change, never on a bare poll.
  const key = exposureSeedKey(current);
  const [seed, setSeed] = useState<{ key: string; draft: ExposureDraft | null }>({ key: "", draft: null });
  if (seed.key !== key) setSeed({ key, draft: current ? toDraft(current) : null });
  const draft = seed.key === key ? seed.draft : current ? toDraft(current) : null;
  const setDraft = (next: ExposureDraft) => setSeed((s) => ({ ...s, draft: next }));
  const [busy, setBusy] = useState(false);

  const request = draft ? exposureRequest(draft) : null;
  const dirty = draft != null && current != null && exposureDirty(draft, current);
  useDirtyGuard(dirty, "settings-capture");

  const apply = async () => {
    if (!request) return;
    setBusy(true);
    try {
      await putCaptureExposure(request);
      toast.ok(t("settings.capture.exposureSaved"));
      capture.refresh();
    } catch (err) {
      toastFromError(err, t("cockpit.settingsSections.capture.exposureFailed"));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SectionGroup title={t("settings.capture.exposureTitle")}>
      {missing ? (
        <EmptyNote>{t("capture.extensionMissing")}</EmptyNote>
      ) : !draft || !current ? (
        <EmptyNote>{capture.stale ? t("capture.loadFailed") : t("common.loading")}</EmptyNote>
      ) : (
        <>
          <SwitchSettingRow
            title={t("settings.capture.locked")}
            summary={exposureNotApplied(current) ? <span className="text-warn">{t("settings.capture.notApplied")}</span> : undefined}
            checked={draft.locked}
            onChange={(locked) => setDraft({ ...draft, locked })}
          />
          <TextSettingRow
            title={t("settings.capture.exposureUs")}
            value={draft.exposureUs}
            inputMode="numeric"
            onChange={(exposureUs) => setDraft({ ...draft, exposureUs })}
          />
          <TextSettingRow
            title={t("settings.capture.gain")}
            value={draft.gain}
            inputMode="decimal"
            onChange={(gain) => setDraft({ ...draft, gain })}
          />
          <SelectSettingRow<ExposureMode | "">
            title={t("settings.capture.mode")}
            options={EXPOSURE_MODES}
            value={draft.mode ?? ""}
            optionLabel={(m) => (m === "" ? t("common.dash") : t(`settings.capture.modes.${m}`))}
            onSelect={(m) => setDraft({ ...draft, mode: m === "" ? null : m })}
          />
          <ApplyBar
            dirty={dirty}
            canApply={request !== null}
            busy={busy}
            onApply={() => void apply()}
            onRevert={() => setDraft(toDraft(current))}
          />
        </>
      )}
    </SectionGroup>
  );
}

export function CaptureSection() {
  return (
    <div className="flex flex-col gap-[0.9rem]">
      <StorageGroup />
      <ExposureGroup />
    </div>
  );
}
