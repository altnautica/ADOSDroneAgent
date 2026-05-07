import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useConfig } from "@/hooks/use-config";
import { ApiError } from "@/lib/api";
import { advancedSectionSchema, postApply } from "@/lib/apply-actions";

const LOG_LEVELS = ["debug", "info", "warning", "error", "critical"] as const;
type LogLevel = (typeof LOG_LEVELS)[number];

export function AdvancedSettings() {
  const config = useConfig();

  const initialLevel = (config.data?.agent?.log_level?.toLowerCase() as LogLevel) ?? "info";
  const initialOverride = config.data?.agent?.board_override ?? "";

  const [logLevel, setLogLevel] = useState<LogLevel>(initialLevel);
  const [boardOverride, setBoardOverride] = useState(initialOverride);

  const [resetConfirmOpen, setResetConfirmOpen] = useState(false);
  const [overrideConfirmOpen, setOverrideConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<{
    kind: "ok" | "err";
    text: string;
  } | null>(null);
  const [validationError, setValidationError] = useState<string | null>(null);

  useEffect(() => {
    if (config.data) {
      setLogLevel((config.data.agent?.log_level?.toLowerCase() as LogLevel) ?? "info");
      setBoardOverride(config.data.agent?.board_override ?? "");
    }
  }, [config.data]);

  async function applyLogLevel(next: LogLevel) {
    const previous = logLevel;
    setLogLevel(next);
    setFeedback(null);
    try {
      const res = await postApply({ advanced: { log_level: next } });
      const section = res.sections.advanced;
      if (!res.overall || !section?.ok) {
        setLogLevel(previous);
        setFeedback({
          kind: "err",
          text: section?.message ?? "Log level update failed.",
        });
      } else {
        setFeedback({
          kind: "ok",
          text: section.message || `Log level set to ${next}.`,
        });
      }
    } catch (err) {
      setLogLevel(previous);
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    }
  }

  async function applyOverride() {
    setBusy(true);
    setFeedback(null);
    try {
      const res = await postApply({
        advanced: { board_override: boardOverride },
      });
      const section = res.sections.advanced;
      if (res.overall && section?.ok) {
        setFeedback({
          kind: "ok",
          text: section.message || "Board override saved.",
        });
        config.refetch();
      } else {
        setFeedback({
          kind: "err",
          text: section?.message ?? "Apply failed.",
        });
      }
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    } finally {
      setBusy(false);
    }
  }

  async function applyFactoryReset() {
    setBusy(true);
    setFeedback(null);
    try {
      const res = await postApply({ advanced: { factory_reset: true } });
      const section = res.sections.advanced;
      if (res.overall && section?.ok) {
        setFeedback({
          kind: "ok",
          text: section.message || "Factory reset queued. Reboot to apply.",
        });
      } else {
        setFeedback({
          kind: "err",
          text: section?.message ?? "Apply failed.",
        });
      }
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
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
            Log level
            <RiskBadge tone="auto" />
          </div>
          <p className="text-xs text-muted-foreground">
            Verbosity for the structured logger. Saved on selection.
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
            Board override
            <RiskBadge tone="manual" />
          </div>
          <p className="text-xs text-muted-foreground">
            Force the HAL detector to use a specific board profile slug.
            Leave blank for auto-detect. Wrong values can disable peripherals.
          </p>
          <div className="space-y-2">
            <Label htmlFor="board-override">Slug</Label>
            <Input
              id="board-override"
              spellCheck={false}
              autoComplete="off"
              maxLength={64}
              placeholder="auto-detect"
              value={boardOverride}
              onChange={(e) => {
                setBoardOverride(e.target.value);
                setValidationError(null);
              }}
            />
          </div>

          {validationError && (
            <div className="rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-700 dark:text-red-300">
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
              Save override
            </Button>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-center gap-2 text-sm font-semibold text-red-600 dark:text-red-400">
            Factory reset
            <RiskBadge tone="manual" />
          </div>
          <p className="text-xs text-muted-foreground">
            Queues a full reset that takes effect on the next reboot.
            Pairing keys, network credentials, and cloud posture all get
            wiped. The agent re-runs setup from scratch.
          </p>
          <div className="flex justify-end">
            <Button
              variant="destructive"
              disabled={busy}
              onClick={() => setResetConfirmOpen(true)}
            >
              Queue factory reset
            </Button>
          </div>
        </CardContent>
      </Card>

      {feedback && (
        <div
          className={`rounded-md border px-3 py-2 text-sm ${
            feedback.kind === "ok"
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300"
          }`}
        >
          {feedback.text}
        </div>
      )}

      <ConfirmDialog
        open={overrideConfirmOpen}
        onOpenChange={setOverrideConfirmOpen}
        title="Override the detected board?"
        description={
          <>
            HAL will use{" "}
            <span className="font-mono font-medium">
              {boardOverride || "auto-detect"}
            </span>{" "}
            on the next config reload. If the slug is wrong, peripherals
            keyed on the board profile (display, GPIO, encoder API) may
            stop working until you clear the override.
          </>
        }
        confirmLabel="Apply override"
        destructive
        onConfirm={applyOverride}
      />

      <ConfirmDialog
        open={resetConfirmOpen}
        onOpenChange={setResetConfirmOpen}
        title="Queue a factory reset?"
        description={
          <>
            This wipes pairing, Wi-Fi credentials, cloud posture, and any
            staged state on the next reboot. There is no undo. The agent
            will reboot into the setup wizard.
          </>
        }
        confirmLabel="Queue reset"
        destructive
        onConfirm={applyFactoryReset}
      />
    </div>
  );
}
