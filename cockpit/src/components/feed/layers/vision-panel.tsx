// The side panel of the cockpit's `vision` view (cockpit-spec views.json):
// the live detection list for the on-screen camera, the depth and surfaces
// layer switches (that view forces them on, so they read on and disabled with
// the reason; a second switch keeps each one's flag for the other views), the
// vision engine's health, and the detector model
// picker. Selecting a model writes `PUT /api/vision/detector`, which restarts
// the engine, so it asks for a second tap within 3 s first, lists detection
// models only (never the depth model), and is disabled while armed.

import { useEffect, useMemo, useState } from "react";

import { apiFetch } from "@/shared/api-fetch";
import { forcingView } from "@/lib/cockpit-view";
import { isDemoMode } from "@/lib/utils";
import { useFlightStore } from "@/stores/flight-store";
import { DEPTH_RANGES, DEPTH_STYLES, useCockpitViewStore } from "@/stores/cockpit-view-store";
import { DETECTION_STALE_MS, useDetectionsStore } from "@/stores/detections-store";

const STATUS_POLL_MS = 2000;
/** The second tap that restarts the engine must come within this window. */
const CONFIRM_WINDOW_MS = 3000;

/** A detector candidate: a detection model (an older agent without `task`
 *  lists detection models only). */
function isDetectionModel(m: { task?: string }): boolean {
  return m.task === undefined || m.task.toLowerCase() === "detection";
}

interface EngineModel {
  id: string;
  kind: string;
  fps: number | null;
  latency_ms: number | null;
  backend_loaded: boolean;
}

interface EngineStatus {
  backend: string | null;
  backend_state: string;
  backend_reason: string | null;
  models: EngineModel[];
}

interface ModelsList {
  installed: { id: string; task?: string }[];
  active: string | null;
}

function useEngineStatus(): { status: EngineStatus | null; error: boolean } {
  const [status, setStatus] = useState<EngineStatus | null>(() =>
    isDemoMode() ? { backend: "mock", backend_state: "running", backend_reason: null, models: [] } : null,
  );
  const [error, setError] = useState(false);
  useEffect(() => {
    if (isDemoMode()) return;
    const ac = new AbortController();
    const poll = () =>
      apiFetch<EngineStatus>("/api/vision/status", { signal: ac.signal }).then(
        (s) => {
          setStatus(s);
          setError(false);
        },
        () => {
          if (!ac.signal.aborted) setError(true);
        },
      );
    void poll();
    const timer = setInterval(() => void poll(), STATUS_POLL_MS);
    return () => {
      ac.abort();
      clearInterval(timer);
    };
  }, []);
  return { status, error };
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
        setErr(e instanceof Error ? e.message : "Could not switch the detector");
        setBusy(false);
      },
    );
  };
  return (
    <section className="space-y-1">
      <h3 className="text-xs uppercase tracking-wider text-white/60">Detector model</h3>
      {models.length === 0 ? <p className="text-xs text-white/60">No detection models installed</p> : null}
      {flying ? <p className="text-xs text-white/60">Disarm to switch the detector: it restarts the vision engine.</p> : null}
      {models.map((m) => (
        <button
          key={m.id}
          type="button"
          disabled={busy || flying || m.id === list.active}
          onClick={() => choose(m.id)}
          className="flex min-h-12 w-full items-center justify-between rounded-md bg-white/5 px-3 text-left text-sm disabled:opacity-60"
        >
          <span className="truncate">{m.id}</span>
          <span className="text-xs text-white/60">
            {m.id === list.active ? "Active" : armed === m.id ? "Tap again to restart engine" : ""}
          </span>
        </button>
      ))}
      {err ? <p className="text-xs text-red-400">{err}</p> : null}
    </section>
  );
}

const RANGE_LABELS: Record<number, string> = { 0: "Off", 2: "2 m", 5: "5 m" };

/** The depth layer's range (Off / 2 m / 5 m) and style (Solid / Outline). */
function DepthOptions() {
  const range = useCockpitViewStore((s) => s.depthRangeM);
  const style = useCockpitViewStore((s) => s.depthStyle);
  const setRange = useCockpitViewStore((s) => s.setDepthRange);
  const setStyle = useCockpitViewStore((s) => s.setDepthStyle);
  const seg = (on: boolean) =>
    `min-h-12 flex-1 rounded-md px-2 text-sm ${on ? "bg-white/25 font-semibold" : "bg-white/5"}`;
  return (
    <div className="space-y-1 pt-1">
      <div role="radiogroup" aria-label="Depth range" className="flex gap-1">
        {DEPTH_RANGES.map((m) => (
          <button key={m} type="button" role="radio" aria-checked={range === m} onClick={() => setRange(m)} className={seg(range === m)}>
            {RANGE_LABELS[m]}
          </button>
        ))}
      </div>
      <div role="radiogroup" aria-label="Depth style" className="flex gap-1">
        {DEPTH_STYLES.map((st) => (
          <button key={st} type="button" role="radio" aria-checked={style === st} onClick={() => setStyle(st)} className={seg(style === st)}>
            {st === "solid" ? "Solid" : "Outline"}
          </button>
        ))}
      </div>
    </div>
  );
}

/** The vision panel's leased layers, with their switch labels. */
const PANEL_LAYERS = [
  { id: "depth", label: "Depth" },
  { id: "surfaces", label: "Surfaces" },
] as const;

/**
 * One layer's switches. While the open view forces the layer on (views.json
 * `forced_layers`, `forcedBy` names that view), its switch reads on and is
 * disabled with the reason; the stored flag, which keeps it on in the other
 * views, stays settable below it.
 */
export function LayerSwitch({
  id,
  label,
  forcedBy,
  stored,
  onToggle,
}: {
  id: string;
  label: string;
  forcedBy: string | null;
  stored: boolean;
  onToggle: (id: string, on: boolean) => void;
}) {
  const reasonId = `layer-${id}-forced`;
  return (
    <div className="rounded-md bg-white/5 px-3 text-sm">
      <label className="flex min-h-12 items-center justify-between">
        <span>{label}</span>
        <input
          type="checkbox"
          checked={forcedBy !== null || stored}
          disabled={forcedBy !== null}
          aria-describedby={forcedBy !== null ? reasonId : undefined}
          onChange={(e) => onToggle(id, e.target.checked)}
        />
      </label>
      {forcedBy !== null ? (
        <>
          <p id={reasonId} className="text-xs text-white/60">
            On while the {forcedBy} view is open
          </p>
          <label className="flex min-h-12 items-center justify-between text-xs text-white/70">
            <span>Keep on in other views</span>
            <input type="checkbox" checked={stored} onChange={(e) => onToggle(id, e.target.checked)} />
          </label>
        </>
      ) : null}
    </div>
  );
}

export function VisionPanel({ cameraId }: { cameraId: string | null }) {
  const batch = useDetectionsStore((s) => (cameraId !== null ? (s.byCamera[cameraId] ?? s.latest) : s.latest));
  const view = useCockpitViewStore((s) => s.view);
  const flags = useCockpitViewStore((s) => s.layers);
  const toggleLayer = useCockpitViewStore((s) => s.toggleLayer);
  const { status, error } = useEngineStatus();
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const fresh = batch !== null && now - batch.receivedAt < DETECTION_STALE_MS;
  const rows = useMemo(
    () => (fresh && batch ? [...batch.detections].sort((a, b) => b.confidence - a.confidence).slice(0, 20) : []),
    [batch, fresh],
  );

  return (
    <aside className="flex h-full w-full flex-col gap-4 overflow-y-auto bg-black/70 p-3 text-white" aria-label="Vision">
      <section className="space-y-1">
        <h3 className="text-xs uppercase tracking-wider text-white/60">Layers</h3>
        {PANEL_LAYERS.map(({ id, label }) => (
          <LayerSwitch
            key={id}
            id={id}
            label={label}
            forcedBy={forcingView(view, id)}
            stored={flags[id] === true}
            onToggle={toggleLayer}
          />
        ))}
        <DepthOptions />
      </section>

      <section className="space-y-1">
        <h3 className="text-xs uppercase tracking-wider text-white/60">Detections</h3>
        {!fresh ? <p className="text-xs text-white/60">No current detections</p> : null}
        <ul className="space-y-0.5 font-mono text-xs">
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

      <section className="space-y-1">
        <h3 className="text-xs uppercase tracking-wider text-white/60">Engine</h3>
        {error && !status ? <p className="text-xs text-red-400">Vision engine unreachable</p> : null}
        {status ? (
          <div className="space-y-0.5 text-xs">
            <div>
              {status.backend ?? "no backend"} · {status.backend_state}
            </div>
            {status.backend_reason ? <div className="text-amber-400">{status.backend_reason}</div> : null}
            {status.models.map((m) => (
              <div key={m.id} className="flex justify-between font-mono">
                <span className="truncate">
                  {m.id} ({m.kind})
                </span>
                <span>{m.fps !== null ? `${m.fps.toFixed(1)} fps` : "idle"}</span>
              </div>
            ))}
          </div>
        ) : null}
      </section>

      <ModelPicker />
    </aside>
  );
}
