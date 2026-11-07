// The side panel of the cockpit's `vision` view (cockpit-spec views.json):
// the live detection list for the on-screen camera, the depth and surfaces
// layer toggles (that view forces them on; the toggles keep their flags for
// the other views), the vision engine's health, and the detector model
// picker. Selecting a model writes `PUT /api/vision/detector`, which restarts
// the engine, so it asks for a second tap first.

import { useEffect, useMemo, useState } from "react";

import { apiFetch } from "@/shared/api-fetch";
import { isDemoMode } from "@/lib/utils";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { DETECTION_STALE_MS, useDetectionsStore } from "@/stores/detections-store";

const STATUS_POLL_MS = 2000;

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
  useEffect(() => {
    if (isDemoMode()) return;
    const ac = new AbortController();
    apiFetch<ModelsList>("/api/vision/models", { signal: ac.signal }).then(setList, () => undefined);
    return () => ac.abort();
  }, []);
  if (!list) return null;
  const choose = (id: string) => {
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
      {list.installed.length === 0 ? <p className="text-xs text-white/60">No models installed</p> : null}
      {list.installed.map((m) => (
        <button
          key={m.id}
          type="button"
          disabled={busy || m.id === list.active}
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

export function VisionPanel({ cameraId }: { cameraId: string | null }) {
  const batch = useDetectionsStore((s) => (cameraId !== null ? (s.byCamera[cameraId] ?? s.latest) : s.latest));
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
        {(["depth", "surfaces"] as const).map((id) => (
          <label key={id} className="flex min-h-12 items-center justify-between rounded-md bg-white/5 px-3 text-sm">
            <span>{id === "depth" ? "Depth" : "Surfaces"}</span>
            <input type="checkbox" checked={flags[id] === true} onChange={(e) => toggleLayer(id, e.target.checked)} />
          </label>
        ))}
        <p className="text-xs text-white/50">On while this view is open; the switches keep them on in other views.</p>
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
