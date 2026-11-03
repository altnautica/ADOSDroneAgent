// Capture — the visor's home: the world-engine extension's capture session.
// REC starts a session for a room and floor, MARK starts a new segment, and
// Stop closes the session (no resume), so Stop fires from a guarded button. A
// node without the extension answers 404 and the screen says so.

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";
import { Circle, Flag, Square } from "lucide-react";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, Dot, EmptyNote, Row, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { TextSettingRow } from "@/components/ui/setting-row";
import { captureControls, captureErrorKey, getCaptureState, startBody } from "@/lib/capture-model";
import type { Tone } from "@/lib/tone";
import { HEALTH_IDS, captureErrorCode, markCapture, startCapture, stopCapture, type HealthLevel } from "@/shared/api/capture";
import { toastFromError } from "@/shared/api/toast";
import { DASH, fmtDuration, fmtInt } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const LEVEL_TONE: Record<HealthLevel, Tone> = { ok: "ok", warn: "warn", bad: "err" };

function healthTone(level: HealthLevel | undefined): Tone {
  return level ? LEVEL_TONE[level] : "muted";
}

export function CaptureScreen() {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  // The operator's edits; null shows the running session's value instead.
  const [roomEdit, setRoom] = useState<string | null>(null);
  const [floorEdit, setFloor] = useState<string | null>(null);

  const capture = useResource(useCallback((s: AbortSignal) => getCaptureState(s), []), 1_000);
  const state = capture.data;
  const room = roomEdit ?? state?.room ?? "";
  const floor = floorEdit ?? (state?.floor != null ? String(state.floor) : "0");

  const run = (action: () => Promise<unknown>) => {
    setBusy(true);
    setErrorCode(null);
    action()
      .then(() => capture.refresh())
      .catch((err: unknown) => {
        const code = captureErrorCode(err);
        if (code) setErrorCode(code);
        else toastFromError(err, t("settings.shared.applyFailed"));
      })
      .finally(() => setBusy(false));
  };

  let body;
  if (!capture.ready) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else if (!state) {
    body = <EmptyNote>{capture.status === 404 ? t("capture.extensionMissing") : t("capture.loadFailed")}</EmptyNote>;
  } else {
    const c = captureControls(state, room, floor, busy);
    const fl = c.floor;
    body = (
      <div className="flex flex-col gap-[0.5rem]">
        <TileGrid>
          <Tile label={t("cockpit.capture.state")} value={t(`capture.state.${state.state}`)} tone={c.recording ? "err" : "muted"} />
          <Tile label={t("cockpit.capture.duration")} value={c.recording ? fmtDuration(state.duration_s) : DASH} />
          <Tile label={t("cockpit.capture.segment")} value={c.recording ? fmtInt(state.segment) : DASH} />
          <Tile label={t("cockpit.capture.frames")} value={fmtInt(state.frames?.total)} />
          <Tile
            label={t("cockpit.capture.storage")}
            value={state.storage ? fmtDuration(state.storage.est_remaining_s) : DASH}
          />
        </TileGrid>
        {state.session_id ? <Row label={t("cockpit.capture.session")} value={state.session_id} /> : null}
        <div className="flex flex-wrap gap-[0.4rem]">
          {HEALTH_IDS.map((id) => {
            const h = state.health.find((x) => x.id === id);
            const tone = healthTone(h?.level);
            return (
              <span key={id} className="flex items-center gap-[0.3rem] rounded-md bg-input/40 px-[0.5rem] py-[0.25rem] text-[0.8rem]">
                <Dot tone={tone} />
                {t(`capture.health.${id}`)}
                <span className="text-muted-foreground">{h ? t(`cockpit.capture.level.${h.level}`) : DASH}</span>
              </span>
            );
          })}
        </div>
        <TextSettingRow title={t("capture.room")} value={room} onChange={setRoom} enabled={!busy} />
        <TextSettingRow title={t("capture.floor")} value={floor} onChange={setFloor} inputMode="numeric" enabled={!busy} />
        <div className="flex flex-wrap gap-[0.5rem]">
          {c.recording ? (
            <GuardedButton
              label={t("capture.stop")}
              guardLabel={t("cockpit.capture.stopGuard")}
              icon={Square}
              disabled={!c.canStop}
              onFired={() => run(stopCapture)}
            />
          ) : (
            <ActionButton
              label={t("capture.rec")}
              icon={Circle}
              variant="primary"
              disabled={!c.canStart}
              onClick={() => fl != null && run(() => startCapture(startBody(room, fl)))}
            />
          )}
          <ActionButton
            label={t("capture.mark")}
            icon={Flag}
            disabled={!c.canMark}
            onClick={() => fl != null && run(() => markCapture({ room: room.trim(), floor: fl }))}
          />
        </div>
        {errorCode ? (
          <div role="alert" className="text-[0.85rem] text-err">
            {captureErrorKey(errorCode) === "other"
              ? t("capture.error.other", { code: errorCode })
              : t(`capture.error.${captureErrorKey(errorCode)}`)}
          </div>
        ) : null}
      </div>
    );
  }

  return (
    <ConsoleScaffold title={t("capture.title")} actions={<StaleBadge stale={capture.stale && capture.data != null} />}>
      {body}
    </ConsoleScaffold>
  );
}
