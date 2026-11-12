import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import { ReadErrorCard } from "@/components/read-error-card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { useResource } from "@/hooks/use-resource";
import { exposureError, gainError } from "@/lib/exposure-draft";
import {
  CAPTURE_STATE_PATH,
  isExtensionMissing,
  putCaptureExposure,
  type CaptureExposure,
  type CaptureState,
} from "@/shared/api/capture";
import { STORAGE_PATH, type StorageStatus } from "@/shared/api/sensors";
import { toast, toastFromError } from "@/shared/api/toast";
import { apiFetch } from "@/shared/api-fetch";
import { fmtNum } from "@/shared/format";

const MODES: NonNullable<CaptureExposure["mode"]>[] = ["walking", "slow", "short"];

function StorageCard() {
  const { t } = useTranslation();
  const storage = useResource<StorageStatus>("storage", STORAGE_PATH, 10_000);
  const s = storage.data;
  const gb = (b: number) => fmtNum(b / 1e9, 1);
  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("settings.capture.storageTitle")}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-2 text-sm">
        {storage.isError ? (
          <ReadErrorCard message={t("settings.capture.storageReadFailed")} onRetry={() => void storage.refetch()} />
        ) : !s ? (
          <p className="text-muted-foreground">{t("common.loading")}</p>
        ) : !s.present ? (
          <p className="text-muted-foreground">{t("settings.capture.storageAbsent")}</p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5">
            <div className="text-xs text-muted-foreground">{t("settings.capture.root")}</div>
            <div className="font-mono text-xs">{s.capture_root ?? t("common.dash")}</div>
            {s.capture && (
              <>
                <div className="text-xs text-muted-foreground">{t("settings.capture.budget")}</div>
                <div className="font-mono text-xs">
                  {t("settings.capture.budgetValue", {
                    used: gb(s.capture.used_bytes),
                    budget: gb(s.capture.budget_bytes),
                    pct: s.capture.budget_pct,
                  })}
                </div>
                <div className="text-xs text-muted-foreground">{t("settings.capture.sessions")}</div>
                <div className="font-mono text-xs">
                  {t("settings.capture.sessionsValue", { sessions: s.capture.sessions, uploaded: s.capture.uploaded })}
                </div>
                <div className="text-xs text-muted-foreground">{t("settings.capture.state")}</div>
                <div>
                  <Badge variant={s.capture.state === "ok" ? "success" : s.capture.state === "full" ? "error" : "warning"}>
                    {s.capture.state}
                  </Badge>
                </div>
              </>
            )}
            {s.write_probe && (
              <>
                <div className="text-xs text-muted-foreground">{t("settings.capture.writeProbe")}</div>
                <div className="font-mono text-xs">{t("settings.capture.mbps", { mbps: fmtNum(s.write_probe.mbps, 0) })}</div>
              </>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function ExposureCard() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const capture = useQuery<CaptureState>({
    queryKey: ["capture-state"],
    queryFn: ({ signal }) => apiFetch<CaptureState>(CAPTURE_STATE_PATH, { signal }),
    retry: (count, err) => !isExtensionMissing(err) && count < 2,
  });
  const current = capture.data?.exposure ?? null;
  // Numbers are kept as typed text so a bad entry shows its error.
  const [draft, setDraft] = useState<
    (Omit<CaptureExposure, "exposure_us" | "gain"> & { exposure_us: string; gain: string }) | null
  >(null);
  const [busy, setBusy] = useState(false);
  const seed = current
    ? {
        ...current,
        exposure_us: current.exposure_us == null ? "" : String(current.exposure_us),
        gain: current.gain == null ? "" : String(current.gain),
      }
    : null;
  useEffect(() => {
    if (seed) setDraft(seed);
    // Re-seed only when the agent's values change, not on every poll.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.locked, current?.exposure_us, current?.gain, current?.mode]);

  async function apply() {
    if (!draft) return;
    setBusy(true);
    try {
      await putCaptureExposure({ ...draft, exposure_us: Number(draft.exposure_us), gain: Number(draft.gain) });
      toast.ok(t("settings.capture.exposureSaved"));
      await qc.invalidateQueries({ queryKey: ["capture-state"] });
    } catch (err) {
      toastFromError(err, t("settings.shared.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  const expErr = draft ? exposureError(draft.exposure_us, t) : null;
  const gainErr = draft ? gainError(draft.gain, t) : null;
  const dirty =
    draft != null &&
    seed != null &&
    (draft.locked !== seed.locked ||
      draft.exposure_us.trim() !== seed.exposure_us ||
      draft.gain.trim() !== seed.gain ||
      draft.mode !== seed.mode);
  const valid = draft != null && expErr === null && gainErr === null && draft.mode != null;

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("settings.capture.exposureTitle")}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        {isExtensionMissing(capture.error) ? (
          <p className="text-muted-foreground">{t("capture.extensionMissing")}</p>
        ) : capture.isError ? (
          <ReadErrorCard message={t("capture.loadFailed")} onRetry={() => void capture.refetch()} />
        ) : !draft ? (
          <p className="text-muted-foreground">{t("common.loading")}</p>
        ) : (
          <>
            <div className="flex items-center gap-3">
              <Switch
                id="exposure-locked"
                checked={draft.locked}
                onCheckedChange={(locked) => setDraft({ ...draft, locked })}
              />
              <Label htmlFor="exposure-locked">{t("settings.capture.locked")}</Label>
              {current?.locked && current.applied === false && (
                <Badge variant="warning">{t("settings.capture.notApplied")}</Badge>
              )}
            </div>
            <div className="flex flex-wrap items-end gap-3">
              <div className="space-y-1">
                <Label htmlFor="exposure-us">{t("settings.capture.exposureUs")}</Label>
                <Input
                  id="exposure-us"
                  className="w-28"
                  inputMode="numeric"
                  value={draft.exposure_us}
                  aria-invalid={expErr !== null}
                  onChange={(e) => setDraft({ ...draft, exposure_us: e.target.value })}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="exposure-gain">{t("settings.capture.gain")}</Label>
                <Input
                  id="exposure-gain"
                  className="w-24"
                  inputMode="decimal"
                  value={draft.gain}
                  aria-invalid={gainErr !== null}
                  onChange={(e) => setDraft({ ...draft, gain: e.target.value })}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="exposure-mode">{t("settings.capture.mode")}</Label>
                <select
                  id="exposure-mode"
                  className="h-9 rounded-md border border-input bg-background px-2 text-sm"
                  value={draft.mode ?? ""}
                  onChange={(e) =>
                    setDraft({ ...draft, mode: (e.target.value || null) as CaptureExposure["mode"] })
                  }
                >
                  {draft.mode == null && <option value="" disabled />}
                  {MODES.map((m) => (
                    <option key={m} value={m}>
                      {t(`settings.capture.modes.${m}`)}
                    </option>
                  ))}
                </select>
              </div>
              <Button size="sm" disabled={busy || !valid || !dirty} onClick={apply}>
                {t("settings.shared.apply")}
              </Button>
            </div>
            {(expErr || gainErr) && (
              <p className="text-xs text-destructive" role="alert">
                {expErr ?? gainErr}
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

export function CaptureSettings() {
  return (
    <div className="space-y-6">
      <StorageCard />
      <ExposureCard />
    </div>
  );
}
