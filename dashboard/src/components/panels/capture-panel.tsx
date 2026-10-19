import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Circle, Flag, Square } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  CAPTURE_STATE_PATH,
  HEALTH_IDS,
  captureErrorCode,
  isExtensionMissing,
  markCapture,
  startCapture,
  stopCapture,
  type CaptureState,
} from "@/lib/capture";
import { toastFromError } from "@/lib/toast";
import { apiFetch } from "@/shared/api-fetch";
import { fmtDuration } from "@/shared/format";

const LEVEL_VARIANT = { ok: "ok", warn: "warn", bad: "err" } as const;
const KNOWN_ERRORS = ["E_STORAGE_FULL", "E_ALREADY_RECORDING", "E_CLOCK_NOT_READY"] as const;
type KnownError = (typeof KNOWN_ERRORS)[number];

export interface CaptureViewProps {
  state: CaptureState;
  /** The last refusal code from the capture service, if any. */
  errorCode: string | null;
  busy: boolean;
  onStart: (room: string, floor: number) => void;
  onStop: () => void;
  onMark: (room: string, floor: number) => void;
}

/** Capture state, REC/STOP, MARK with room and floor, and the health chips. */
export function CaptureView({ state, errorCode, busy, onStart, onStop, onMark }: CaptureViewProps) {
  const { t } = useTranslation();
  const [room, setRoom] = useState(state.room ?? "");
  const [floor, setFloor] = useState(String(state.floor ?? 0));
  const floorNum = Number.parseInt(floor, 10);
  const floorValid = Number.isFinite(floorNum);
  const recording = state.state === "recording" || state.state === "paused";

  return (
    <div className="space-y-4 text-sm">
      <div className="flex flex-wrap items-center gap-3">
        <Badge variant={recording ? "err" : "default"}>{t(`capture.state.${state.state}`)}</Badge>
        {state.session_id && <span className="font-mono text-xs">{state.session_id}</span>}
        {recording && (
          <span className="font-mono text-xs">
            {t("capture.progress", {
              duration: fmtDuration(state.duration_s),
              segment: state.segment,
              frames: state.frames?.total ?? 0,
            })}
          </span>
        )}
      </div>
      <div className="flex flex-wrap gap-2">
        {HEALTH_IDS.map((id) => {
          const h = state.health.find((x) => x.id === id);
          return (
            <Badge key={id} variant={h ? LEVEL_VARIANT[h.level] : "default"}>
              {t(`capture.health.${id}`)}
            </Badge>
          );
        })}
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1">
          <Label htmlFor="capture-room">{t("capture.room")}</Label>
          <Input id="capture-room" className="w-40" value={room} onChange={(e) => setRoom(e.target.value)} />
        </div>
        <div className="space-y-1">
          <Label htmlFor="capture-floor">{t("capture.floor")}</Label>
          <Input
            id="capture-floor"
            className="w-20"
            inputMode="numeric"
            value={floor}
            onChange={(e) => setFloor(e.target.value)}
          />
        </div>
        {recording ? (
          <Button size="sm" variant="destructive" disabled={busy} onClick={onStop}>
            <Square className="h-3.5 w-3.5" />
            {t("capture.stop")}
          </Button>
        ) : (
          <Button size="sm" disabled={busy || !floorValid} onClick={() => onStart(room, floorNum)}>
            <Circle className="h-3.5 w-3.5" />
            {t("capture.rec")}
          </Button>
        )}
        <Button
          size="sm"
          variant="outline"
          disabled={busy || !recording || !room.trim() || !floorValid}
          onClick={() => onMark(room.trim(), floorNum)}
        >
          <Flag className="h-3.5 w-3.5" />
          {t("capture.mark")}
        </Button>
      </div>
      {errorCode && (
        <p className="text-xs text-destructive" role="alert">
          {KNOWN_ERRORS.includes(errorCode as KnownError)
            ? t(`capture.error.${errorCode as KnownError}`)
            : t("capture.error.other", { code: errorCode })}
        </p>
      )}
      {state.storage && (
        <p className="text-xs text-muted-foreground">
          {t("capture.storage", { remaining: fmtDuration(state.storage.est_remaining_s) })}
        </p>
      )}
    </div>
  );
}

export function CapturePanel() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const capture = useQuery<CaptureState>({
    queryKey: ["capture-state"],
    queryFn: ({ signal }) => apiFetch<CaptureState>(CAPTURE_STATE_PATH, { signal }),
    refetchInterval: 1_000,
    retry: (count, err) => !isExtensionMissing(err) && count < 2,
  });

  async function run(action: () => Promise<unknown>) {
    setBusy(true);
    setErrorCode(null);
    try {
      await action();
      await qc.invalidateQueries({ queryKey: ["capture-state"] });
    } catch (err) {
      const code = captureErrorCode(err);
      if (code) setErrorCode(code);
      else toastFromError(err, t("settings.shared.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("capture.title")}</CardTitle>
      </CardHeader>
      <CardContent>
        {capture.data ? (
          <CaptureView
            state={capture.data}
            errorCode={errorCode}
            busy={busy}
            onStart={(room, floor) => run(() => startCapture(room.trim() ? { room: room.trim(), floor } : { floor }))}
            onStop={() => run(stopCapture)}
            onMark={(room, floor) => run(() => markCapture({ room, floor }))}
          />
        ) : (
          <p className="text-sm text-muted-foreground">
            {isExtensionMissing(capture.error)
              ? t("capture.extensionMissing")
              : capture.isError
                ? t("capture.loadFailed")
                : t("common.loading")}
          </p>
        )}
      </CardContent>
    </Card>
  );
}
