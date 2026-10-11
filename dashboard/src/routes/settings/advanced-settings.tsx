import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useConfig } from "@/hooks/use-config";
import { advancedSectionSchema, postApply } from "@/lib/apply-actions";
import { toast, toastFromError } from "@/lib/toast";

const LOG_LEVELS = ["debug", "info", "warning", "error", "critical"] as const;
type LogLevel = (typeof LOG_LEVELS)[number];

export function AdvancedSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  const initialLevel = (config.data?.logging?.level?.toLowerCase() as LogLevel) ?? "info";
  const initialOverride = config.data?.agent?.board_override ?? "";

  const [logLevel, setLogLevel] = useState<LogLevel>(initialLevel);
  const [boardOverride, setBoardOverride] = useState(initialOverride);

  const [overrideConfirmOpen, setOverrideConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);

  useEffect(() => {
    if (config.data) {
      setLogLevel((config.data.logging?.level?.toLowerCase() as LogLevel) ?? "info");
      setBoardOverride(config.data.agent?.board_override ?? "");
    }
  }, [config.data]);

  async function applyLogLevel(next: LogLevel) {
    const previous = logLevel;
    setLogLevel(next);
    try {
      const res = await postApply({ advanced: { log_level: next } });
      const section = res.sections.advanced;
      if (!res.overall || !section?.ok) {
        setLogLevel(previous);
        toast.err(section?.message ?? t("settingsMore.advanced.logLevelFailed"));
      } else {
        toast.ok(section.message || t("settingsMore.advanced.logLevelSet", { level: next }));
      }
    } catch (err) {
      setLogLevel(previous);
      toastFromError(err, t("settingsMore.advanced.logLevelFailed"));
    }
  }

  async function applyOverride() {
    setBusy(true);
    try {
      const res = await postApply({
        advanced: { board_override: boardOverride },
      });
      const section = res.sections.advanced;
      if (res.overall && section?.ok) {
        toast.ok(section.message || t("settingsMore.advanced.overrideSaved"));
        config.refetch();
      } else {
        toast.err(section?.message ?? t("settingsMore.cloud.applyFailed"));
      }
    } catch (err) {
      toastFromError(err, t("settingsMore.cloud.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  function validateOverride(): boolean {
    const result = advancedSectionSchema.safeParse({
      board_override: boardOverride,
    });
    if (!result.success) {
      const first = result.error.issues[0];
      setValidationError(`${first.path.join(".")}: ${first.message}`);
      return false;
    }
    setValidationError(null);
    return true;
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-center gap-2 text-sm font-semibold">
            {t("settingsMore.advanced.logLevel")}
            <RiskBadge tone="auto" />
          </div>
          <p className="text-xs text-muted-foreground">
            {t("settingsMore.advanced.logLevelHint")}
          </p>
          <div className="flex flex-wrap gap-2">
            {LOG_LEVELS.map((lvl) => {
              const active = lvl === logLevel;
              return (
                <button
                  key={lvl}
                  type="button"
                  onClick={() => {
                    if (lvl !== logLevel) applyLogLevel(lvl);
                  }}
                  className={`px-3 py-1 rounded-md border text-xs font-mono transition-colors ${
                    active
                      ? "border-primary bg-primary/15 text-primary"
                      : "border-border bg-background text-muted-foreground hover:bg-accent/40"
                  }`}
                >
                  {lvl}
                </button>
              );
            })}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-center gap-2 text-sm font-semibold">
            {t("settingsMore.advanced.boardOverride")}
            <RiskBadge tone="manual" />
          </div>
          <p className="text-xs text-muted-foreground">
            {t("settingsMore.advanced.boardOverrideHint")}
          </p>
          <div className="space-y-2">
            <Label htmlFor="board-override">{t("settingsMore.advanced.slug")}</Label>
            <Input
              id="board-override"
              spellCheck={false}
              autoComplete="off"
              maxLength={64}
              placeholder={t("settingsMore.advanced.autoDetect")}
              value={boardOverride}
              onChange={(e) => {
                setBoardOverride(e.target.value);
                setValidationError(null);
              }}
            />
          </div>

          {validationError && (
            <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
              {validationError}
            </div>
          )}

          <div className="flex justify-end">
            <Button
              variant="default"
              disabled={boardOverride === initialOverride || busy}
              onClick={() => {
                if (validateOverride()) setOverrideConfirmOpen(true);
              }}
            >
              {t("settingsMore.advanced.saveOverride")}
            </Button>
          </div>
        </CardContent>
      </Card>

      <ConfirmDialog
        open={overrideConfirmOpen}
        onOpenChange={setOverrideConfirmOpen}
        title={t("settingsMore.advanced.confirmTitle")}
        description={
          <Trans
            i18nKey="settingsMore.advanced.confirmBody"
            values={{ slug: boardOverride || t("settingsMore.advanced.autoDetect") }}
            components={{ slug: <span className="font-mono font-medium" /> }}
          />
        }
        confirmLabel={t("settingsMore.advanced.applyOverride")}
        destructive
        onConfirm={applyOverride}
      />
    </div>
  );
}
