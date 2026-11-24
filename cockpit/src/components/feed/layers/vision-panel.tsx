// The side panel of the cockpit's `vision` view (cockpit-spec views.json).
// Top to bottom:
// - a header with a close button (and Esc) back to the Pilot view;
// - the vision engine card (vision-engine-card.tsx): its state and the one
//   action that moves it forward;
// - one row per leased layer: its switch, name and live status (receiving,
//   no data, engine off). That view forces depth and surfaces on, so their
//   switches read on and disabled with the reason, and a small checkbox keeps
//   each one's flag for the other views. Depth's range and style sit inside
//   its row, behind a disclosure shown while depth is on;
// - the live detection list for the on-screen camera;
// - the detector model picker. Selecting a model writes
//   `PUT /api/vision/detector`, which restarts the engine, so it asks for a
//   second tap within 3 s first, lists detection models only (never the depth
//   model), and is disabled while armed.
// The header stays put; everything under it scrolls in the panel's column.

import { ChevronDown, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { useMapChrome } from "@/hooks/use-map-chrome";
import { useNow } from "@/hooks/use-now";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { forcingView, layerFlag } from "@/lib/cockpit-view";
import { layerFrames } from "@/lib/layer-frames";
import { cn, isDemoMode } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import { fmtDistance } from "@/shared/hud/format";
import { specText } from "@/shared/hud/spec-text";
import { DEPTH_RANGES, DEPTH_STYLES, useCockpitViewStore } from "@/stores/cockpit-view-store";
import { DETECTION_STALE_MS, useDetectionsStore } from "@/stores/detections-store";
import { useFlightStore } from "@/stores/flight-store";
import { useLayerChipsStore, visionEngineUp } from "@/stores/layer-chips-store";

import { useVisionEngine } from "./use-vision-engine";
import { VisionEngineCard } from "./vision-engine-card";
import { layerLiveState, type LayerLiveState } from "./vision-state";

/** The second tap that restarts the engine must come within this window. */
const CONFIRM_WINDOW_MS = 3000;

const SECTION_TITLE = "text-[0.85em] uppercase tracking-wider text-white/60";

/** A detector candidate: a detection model (an older agent without `task`
 *  lists detection models only). */
function isDetectionModel(m: { task?: string }): boolean {
  return m.task === undefined || m.task.toLowerCase() === "detection";
}

interface ModelsList {
  installed: { id: string; task?: string }[];
  active: string | null;
}

function ModelPicker() {
  const [list, setList] = useState<ModelsList | null>(null);
  const [armed, setArmed] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const flying = useFlightStore((s) => s.telemetry?.armed === true);
  useEffect(() => {
    if (armed === null) return;
    const t = setTimeout(() => setArmed(null), CONFIRM_WINDOW_MS);
    return () => clearTimeout(t);
  }, [armed]);
  useEffect(() => {
    if (isDemoMode()) return;
    const ac = new AbortController();
    apiFetch<ModelsList>("/api/vision/models", { signal: ac.signal }).then(setList, () => undefined);
    return () => ac.abort();
  }, []);
  if (!list) return null;
  const models = list.installed.filter(isDetectionModel);
  const choose = (id: string) => {
    if (flying) return;
    if (armed !== id) {
      setArmed(id);
      return;
    }
    setBusy(true);
    setErr(null);
    apiFetch<{ model_id: string | null }>("/api/vision/detector", { method: "PUT", body: { model_id: id } }).then(
      (r) => {
        setList({ ...list, active: r.model_id });
        setArmed(null);
        setBusy(false);
      },
      (e: unknown) => {
        setErr(e instanceof Error ? e.message : specText("vision.switch_failed"));
        setBusy(false);
      },
    );
  };
  return (
    <section className="space-y-[4px]">
      <h3 className={SECTION_TITLE}>{specText("vision.detector_model")}</h3>
      {models.length === 0 ? <p className="text-[0.85em] text-white/60">{specText("vision.no_models")}</p> : null}
      {flying ? <p className="text-[0.85em] text-white/60">{specText("vision.disarm_to_switch")}</p> : null}
      {models.map((m) => (
        <button
          key={m.id}
          type="button"
          disabled={busy || flying || m.id === list.active}
          onClick={() => choose(m.id)}
          className="flex min-h-[48px] w-full items-center justify-between rounded-md bg-white/5 px-[12px] text-left text-[1em] disabled:opacity-60"
        >
          <span className="truncate">{m.id}</span>
          <span className="text-[0.85em] text-white/60">
            {m.id === list.active ? specText("vision.active") : armed === m.id ? specText("vision.restart_hint") : ""}
          </span>
        </button>
      ))}
      {err ? <p className="text-[0.85em] text-red-400">{err}</p> : null}
    </section>
  );
}

const RANGE_LABELS: Record<number, string> = { 0: specText("vision.off"), 2: fmtDistance(2), 5: fmtDistance(5) };

/** The depth layer's range (Off / 2 m / 5 m) and style (Solid / Outline). */
function DepthOptions() {
  const range = useCockpitViewStore((s) => s.depthRangeM);
  const style = useCockpitViewStore((s) => s.depthStyle);
  const setRange = useCockpitViewStore((s) => s.setDepthRange);
  const setStyle = useCockpitViewStore((s) => s.setDepthStyle);
  const seg = (on: boolean) =>
    `min-h-[48px] flex-1 rounded-md px-[8px] text-[1em] ${on ? "bg-white/25 font-semibold" : "bg-white/5"}`;
  return (
    <div className="space-y-[4px] pb-[8px]">
      <div role="radiogroup" aria-label={specText("vision.depth_range")} className="flex gap-[4px]">
        {DEPTH_RANGES.map((m) => (
          <button key={m} type="button" role="radio" aria-checked={range === m} onClick={() => setRange(m)} className={seg(range === m)}>
            {RANGE_LABELS[m]}
          </button>
        ))}
      </div>
      <div role="radiogroup" aria-label={specText("vision.depth_style")} className="flex gap-[4px]">
        {DEPTH_STYLES.map((st) => (
          <button key={st} type="button" role="radio" aria-checked={style === st} onClick={() => setStyle(st)} className={seg(style === st)}>
            {specText(st === "solid" ? "vision.depth_style.solid" : "vision.depth_style.outline")}
          </button>
        ))}
      </div>
    </div>
  );
}

/** The vision panel's leased layers, with their switch labels. */
const PANEL_LAYERS = [
  { id: "depth", label: specText("vision.depth") },
  { id: "surfaces", label: specText("vision.surfaces") },
] as const;

const LIVE_DOT: Readonly<Record<LayerLiveState, string>> = {
  receiving: "bg-emerald-400",
  no_data: "bg-amber-400",
  engine_off: "bg-white/40",
};

/**
 * One layer's row: switch, name and live status. While the open view forces
 * the layer on (views.json `forced_layers`, `forcedBy` names that view), its
 * switch reads on and is disabled with the reason as its helper text; the
 * stored flag, which keeps it on in the other views, stays settable as a
 * small checkbox under it. `children` (the layer's options) show while the
 * layer is on, behind a disclosure.
 */
export function LayerSwitch({
  id,
  label,
  forcedBy,
  stored,
  live,
  onToggle,
  optionsLabel,
  children,
}: {
  id: string;
  label: string;
  forcedBy: string | null;
  stored: boolean;
  /** The live status, shown while the layer is on. */
  live?: LayerLiveState;
  onToggle: (id: string, on: boolean) => void;
  optionsLabel?: string;
  children?: ReactNode;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const reasonId = `layer-${id}-forced`;
  const on = forcedBy !== null || stored;
  const liveText: Readonly<Record<LayerLiveState, string>> = {
    receiving: t("vision.layer.receiving"),
    no_data: t("vision.layer.noData"),
    engine_off: t("vision.layer.engineOff"),
  };
  return (
    <div className="rounded-md bg-white/5 px-[12px] text-[1em]" data-testid={`layer-row-${id}`}>
      <label className="flex min-h-[48px] items-center justify-between gap-[8px]">
        <span className="flex min-w-0 flex-col">
          <span>{label}</span>
          {on && live ? (
            <span className="flex items-center gap-[6px] text-[0.8em] text-white/60">
              <span className={cn("h-[0.6em] w-[0.6em] rounded-full", LIVE_DOT[live])} aria-hidden />
              {liveText[live]}
            </span>
          ) : null}
        </span>
        <input
          type="checkbox"
          role="switch"
          className="h-[24px] w-[24px] shrink-0"
          checked={on}
          disabled={forcedBy !== null}
          aria-describedby={forcedBy !== null ? reasonId : undefined}
          onChange={(e) => onToggle(id, e.target.checked)}
        />
      </label>
      {forcedBy !== null ? (
        <>
          <p id={reasonId} className="text-[0.85em] text-white/60">
            {specText("vision.on_while_view", { view: forcedBy })}
          </p>
          <label className="flex min-h-[48px] items-center gap-[8px] text-[0.85em] text-white/70">
            <input type="checkbox" checked={stored} onChange={(e) => onToggle(id, e.target.checked)} />
            <span>{specText("vision.keep_on")}</span>
          </label>
        </>
      ) : null}
      {on && children ? (
        <>
          <button
            type="button"
            aria-expanded={open}
            onClick={() => setOpen(!open)}
            className="flex min-h-[48px] w-full items-center justify-between text-[0.85em] text-white/70"
          >
            <span>{optionsLabel}</span>
            <ChevronDown className={cn("h-[1em] w-[1em] transition-transform", open && "rotate-180")} aria-hidden />
          </button>
          {open ? children : null}
        </>
      ) : null}
    </div>
  );
}

/** Close the Vision view: back to the Pilot view. */
function closeVision(): void {
  useCockpitViewStore.getState().setView("pilot");
}

export function VisionPanel({ cameraId }: { cameraId: string | null }) {
  const { t } = useTranslation();
  const batch = useDetectionsStore((s) => (cameraId !== null ? (s.byCamera[cameraId] ?? s.latest) : s.latest));
  const view = useCockpitViewStore((s) => s.view);
  const flags = useCockpitViewStore((s) => s.layers);
  const toggleLayer = useCockpitViewStore((s) => s.toggleLayer);
  const engineUp = useLayerChipsStore(visionEngineUp);
  useVisionEngine(true);
  const rootRef = useRef<HTMLElement>(null);
  const { scale } = useMapChrome(rootRef);
  const now = useNow(1000);
  const mono = useNow(1000, "monotonic");
  const fresh = batch !== null && now - batch.receivedAt < DETECTION_STALE_MS;
  const rows = useMemo(
    () => (fresh && batch ? [...batch.detections].sort((a, b) => b.confidence - a.confidence).slice(0, 20) : []),
    [batch, fresh],
  );

  // Esc closes the panel, unless something above it (a menu, a sheet) took
  // the key first or the operator is typing.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT")) return;
      closeVision();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const slots = { depth: layerFrames.depth, surfaces: layerFrames.surfaces } as const;

  // Text in chrome px times the class scale; the rows size in em from it.
  return (
    <aside
      ref={rootRef}
      className="flex h-full w-full flex-col bg-black/70 text-white"
      style={{ fontSize: CHROME_TOKENS.type.chip_px * scale }}
      aria-label={specText("vision.title")}
      data-testid="vision-panel"
    >
      <header className="flex shrink-0 items-center justify-between border-b border-white/10 pl-[12px]">
        <h2 className="text-[1.1em] font-semibold">{specText("vision.title")}</h2>
        <button
          type="button"
          onClick={closeVision}
          aria-label={t("vision.close")}
          className="flex h-[48px] w-[48px] items-center justify-center rounded-md hover:bg-white/10"
          data-testid="vision-close"
        >
          <X className="h-[1.3em] w-[1.3em]" aria-hidden />
        </button>
      </header>

      <div className="flex min-h-0 flex-1 flex-col gap-[16px] overflow-y-auto p-[12px]">
        <VisionEngineCard />

        <section className="space-y-[4px]">
          <h3 className={SECTION_TITLE}>{specText("vision.layers")}</h3>
          {PANEL_LAYERS.map(({ id, label }) => (
            <LayerSwitch
              key={id}
              id={id}
              label={label}
              forcedBy={forcingView(view, id)}
              stored={layerFlag(flags, id)}
              live={layerLiveState(slots[id].seen, slots[id].data !== null, mono, engineUp)}
              onToggle={toggleLayer}
              optionsLabel={id === "depth" ? t("vision.layer.depthOptions") : undefined}
            >
              {id === "depth" ? <DepthOptions /> : null}
            </LayerSwitch>
          ))}
        </section>

        <section className="space-y-[4px]">
          <h3 className={SECTION_TITLE}>
            {specText("vision.detections")}
            {fresh ? ` · ${t("vision.detections.count", { count: batch?.detections.length ?? 0 })}` : ""}
          </h3>
          {!fresh || rows.length === 0 ? <p className="text-[0.85em] text-white/60">{t("vision.detections.empty")}</p> : null}
          <ul className="space-y-[2px] font-mono text-[0.85em]">
            {rows.map((d, i) => (
              <li key={d.trackId ?? `i${i}`} className="flex justify-between">
                <span>
                  {d.classLabel}
                  {d.trackId != null ? ` #${d.trackId}` : ""}
                </span>
                <span>{Math.round(d.confidence * 100)}%</span>
              </li>
            ))}
          </ul>
        </section>

        <ModelPicker />
      </div>
    </aside>
  );
}
