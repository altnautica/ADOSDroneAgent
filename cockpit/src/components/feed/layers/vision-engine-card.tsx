// The Vision panel's engine card, first in the panel: a status dot and one
// line saying what the vision engine is doing, and the one action that moves
// it forward.
//
// - Running: the backend and the detector model.
// - Off (`vision.enabled` false): Turn on vision writes the flag, restarts the
//   `ados-vision` unit, then the shared 2 s status poll watches it come up
//   for up to 30 s.
// - Not responding while enabled, or its backend unavailable: Restart vision.
// - A build without the backend's runtime: says so, with no action, since no
//   restart can bring it up.
// - An agent that refuses to restart the unit on its own: vision starts after
//   the agent restarts, so the card offers that restart instead (disarmed only).
// - Armed: every action is disabled with the reason.

import { Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import { useAgentConfig, useLeafWriter } from "@/components/settings/sections/section-hooks";
import { useNow } from "@/hooks/use-now";
import { VISION_KEYS } from "@/lib/settings/vision";
import { cn } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import { specText } from "@/shared/hud/spec-text";
import { useFlightStore } from "@/stores/flight-store";
import { useLayerChipsStore } from "@/stores/layer-chips-store";

import { pollVisionNow } from "./use-vision-engine";
import { engineCardState, engineRunning, restartVisionUnit, type EngineCardState, type EnginePending } from "./vision-state";

const DOT: Readonly<Record<EngineCardState, string>> = {
  running: "bg-emerald-400",
  starting: "bg-amber-400 animate-pulse",
  agent_restarting: "bg-amber-400 animate-pulse",
  off: "bg-white/40",
  checking: "bg-white/40",
  needs_agent_restart: "bg-amber-400",
  unreachable: "bg-red-400",
  unavailable: "bg-red-400",
  no_runtime: "bg-red-400",
  start_timeout: "bg-red-400",
};

const PRIMARY = "min-h-[48px] w-full rounded-md bg-white/90 px-[12px] text-[1em] font-semibold text-black disabled:opacity-50";

export function VisionEngineCard() {
  const { t } = useTranslation();
  const status = useLayerChipsStore((s) => s.vision);
  const error = useLayerChipsStore((s) => s.visionError);
  const { config, get } = useAgentConfig();
  const enabled = config == null ? null : get<boolean | null>(VISION_KEYS.enabled, null);
  const { save } = useLeafWriter();
  const armed = useFlightStore((s) => s.telemetry?.armed === true);
  const [pending, setPending] = useState<EnginePending>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const now = useNow(1000);
  const state = engineCardState({ enabled, status, error, pending, now });

  // Up: the action in flight is done.
  useEffect(
    () =>
      useLayerChipsStore.subscribe((s) => {
        if (engineRunning(s.vision, s.visionError)) setPending(null);
      }),
    [],
  );

  const fail = (e: unknown) => {
    setPending(null);
    setFailure(t("vision.engine.failed", { error: e instanceof Error ? e.message : String(e) }));
  };

  const restart = async () => {
    setFailure(null);
    setPending({ kind: "starting", since: Date.now() });
    try {
      if ((await restartVisionUnit()) === "refused") setPending({ kind: "needs_agent_restart" });
    } catch (e) {
      fail(e);
    }
    pollVisionNow();
  };

  const turnOn = async () => {
    setFailure(null);
    if (await save(VISION_KEYS.enabled, true, { quiet: true })) await restart();
  };

  const restartAgent = async () => {
    setFailure(null);
    setPending({ kind: "agent_restarting", since: Date.now() });
    try {
      await apiFetch("/api/v1/system/restart-supervisor", { method: "POST", body: {} });
    } catch (e) {
      fail(e);
    }
  };

  const detector = status?.models.find((m) => m.kind.toLowerCase() === "detection")?.id ?? status?.models[0]?.id ?? null;
  const line: Readonly<Record<EngineCardState, string>> = {
    running: t("vision.engine.running"),
    starting: t("vision.engine.starting"),
    agent_restarting: t("vision.engine.agentRestarting"),
    off: t("vision.engine.off"),
    checking: t("common.loading"),
    needs_agent_restart: t("vision.engine.needsAgentRestart"),
    unreachable: t("vision.engine.unreachable"),
    unavailable: t("vision.engine.unavailable"),
    no_runtime: t("vision.engine.noRuntime"),
    start_timeout: t("vision.engine.startTimeout"),
  };
  const busy = state === "starting" || state === "agent_restarting";

  return (
    <section className="space-y-[8px] rounded-md bg-white/5 p-[12px]" data-testid="vision-engine-card" data-state={state}>
      <h3 className="text-[0.85em] uppercase tracking-wider text-white/60">{t("vision.engine.title")}</h3>
      <div className="flex items-center gap-[8px]">
        {busy ? (
          <Loader2 className="h-[1em] w-[1em] animate-spin text-amber-400" aria-hidden />
        ) : (
          <span className={cn("h-[0.7em] w-[0.7em] shrink-0 rounded-full", DOT[state])} aria-hidden />
        )}
        <span className="font-semibold">{line[state]}</span>
      </div>
      {state === "running" ? (
        <div className="space-y-[2px] text-[0.85em] text-white/80">
          <div>{[status?.backend ?? specText("vision.no_backend"), detector].filter(Boolean).join(" · ")}</div>
          {status?.models.map((m) => (
            <div key={m.id} className="flex justify-between font-mono">
              <span className="truncate">
                {m.id} ({m.kind})
              </span>
              <span>{m.fps !== null ? specText("vision.fps", { n: m.fps.toFixed(1) }) : specText("vision.idle")}</span>
            </div>
          ))}
        </div>
      ) : null}
      {state === "off" ? <p className="text-[0.85em] text-white/60">{t("vision.engine.offHint")}</p> : null}
      {state === "unavailable" && status?.backend_reason ? <p className="text-[0.85em] text-amber-400">{status.backend_reason}</p> : null}
      {state === "no_runtime" ? <p className="text-[0.85em] text-white/60">{t("vision.engine.noRuntimeHint")}</p> : null}
      {state === "off" ? (
        <button type="button" className={PRIMARY} disabled={armed} onClick={() => void turnOn()}>
          {t("vision.engine.turnOn")}
        </button>
      ) : null}
      {state === "unreachable" || state === "unavailable" || state === "start_timeout" ? (
        <button type="button" className={PRIMARY} disabled={armed} onClick={() => void restart()}>
          {t("vision.engine.restart")}
        </button>
      ) : null}
      {state === "needs_agent_restart" ? (
        <button type="button" className={PRIMARY} disabled={armed} onClick={() => void restartAgent()}>
          {t("vision.engine.restartAgent")}
        </button>
      ) : null}
      {armed && state !== "running" && state !== "no_runtime" && !busy ? (
        <p className="text-[0.85em] text-white/60">{t("vision.engine.armed")}</p>
      ) : null}
      {failure ? <p className="text-[0.85em] text-red-400">{failure}</p> : null}
    </section>
  );
}
