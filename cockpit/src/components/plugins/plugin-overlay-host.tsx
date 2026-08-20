// Plugin-contribution mount for the on-box cockpit.
//
// The cockpit serves a plugin's GCS surfaces inside a sandboxed iframe, exactly
// as Mission Control does. The iframe is `sandbox="allow-scripts"` with a null
// origin and no network, so the plugin's GCS bundle cannot call out; all I/O
// round-trips a postMessage bridge using the same `RpcEnvelope
// {id,type,method,capability,args,version:1}` contract the plugin SDK speaks.
// One bundle therefore runs unchanged in both Mission Control and this
// cockpit.
//
// Because a sandboxed iframe cannot fetch its own bundle (no network), the host
// fetches the bundle text from the agent's traversal-guarded
// `GET /api/plugins/{id}/gcs/{entrypoint}` route and inlines it via `srcDoc`.
// The host answers the plugin's `perception.*` reads from the cockpit's own
// detection store, so the lock panel and HUD update with live boxes exactly as
// they do over the Mission Control bridge.

import { useEffect, useMemo, useRef, useState } from "react";

import { apiFetch } from "@/lib/api";
import { useDetectionsStore } from "@/stores/detections-store";
import type { CockpitDetectionBatch } from "@/stores/detections-store";

/** Plugin RPC protocol version (mirrors the SDK's `PROTOCOL_VERSION`). */
const PROTOCOL_VERSION = 1;

/** Plugin id + GCS entrypoint the cockpit mounts. Only plugins declaring a GCS
 *  half with an entrypoint are candidates. */
export interface PluginOverlayHostProps {
  pluginId: string;
  entrypoint: string;
}

interface RpcEnvelope {
  id?: string;
  type?: "request" | "response" | "event";
  method?: string;
  capability?: string;
  args?: unknown;
  version?: number;
  error?: { code: string; message: string };
}

/** Map one cockpit batch onto the SDK's `PerceptionDetectionBatch` shape (the
 *  `perception.detections` event payload the plugin already decodes in MC). */
function toPerceptionBatch(b: CockpitDetectionBatch): Record<string, unknown> {
  return {
    modelId: b.modelId,
    cameraId: b.cameraId,
    frameId: b.frameId,
    tsMs: b.tsMs,
    frameWidth: b.frameWidth,
    frameHeight: b.frameHeight,
    detections: b.detections.map((d) => ({
      bbox: d.bbox
        ? { x: d.bbox.x, y: d.bbox.y, width: d.bbox.width, height: d.bbox.height }
        : undefined,
      classLabel: d.classLabel,
      confidence: d.confidence,
      trackId: d.trackId ?? null,
      lockState: d.lockState ?? null,
    })),
  };
}

/** The cockpit's amber-on-charcoal palette as the hex vars the plugin reads. */
function themeVars(): Record<string, string> {
  return {
    "--color-accent-primary": "#e7c254",
    "--color-text-primary": "#e0e0e0",
  };
}

export function PluginOverlayHost({
  pluginId,
  entrypoint,
}: PluginOverlayHostProps) {
  const [bundle, setBundle] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const iframeRef = useRef<HTMLIFrameElement>(null);

  const bundleUrl = `/api/plugins/${encodeURIComponent(
    pluginId,
  )}/gcs/${encodeURIComponent(entrypoint)}`;

  useEffect(() => {
    let cancelled = false;
    apiFetch<string>(bundleUrl, {})
      .then((text) => {
        if (!cancelled) setBundle(typeof text === "string" ? text : String(text));
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [bundleUrl]);

  const srcDoc = useMemo(() => {
    if (!bundle) return "";
    return `<!doctype html><html><head><meta charset="utf-8"></head><body style="margin:0;width:100%;height:100%;background:transparent;color:#e0e0e0;font-family:system-ui,sans-serif;">${bundle
      .replace(/<\/script>/gi, "<\\/script>")
      .replace(/<!--/g, "<\\!--")}</body></html>`;
  }, [bundle]);

  useEffect(() => {
    if (!srcDoc) return;
    const iframe = iframeRef.current;
    if (!iframe) return;
    const win = iframe.contentWindow;
    if (!win) return;

    const send = (env: RpcEnvelope) => win.postMessage(env, "*");

    // Push theme tokens on load and whenever the store's batch changes.
    send({ type: "event", method: "theme.changed", capability: "theme", args: themeVars(), version: PROTOCOL_VERSION });

    const pushBatch = (batch: CockpitDetectionBatch | null) => {
      const payload = batch ? toPerceptionBatch(batch) : { detections: [] };
      send({ type: "event", method: "perception.detections", capability: "perception.subscribe", args: payload, version: PROTOCOL_VERSION });
    };
    pushBatch(useDetectionsStore.getState().latest);
    const unsubscribe = useDetectionsStore.subscribe((s, prev) => {
      if (s.latest !== prev.latest) pushBatch(s.latest);
    });

    const handler = (ev: MessageEvent<RpcEnvelope>) => {
      if (ev.source !== iframe.contentWindow) return;
      const env = ev.data;
      if (!env || typeof env !== "object" || env.version !== PROTOCOL_VERSION) return;
      if (env.type !== "request" || !env.method || env.id === undefined) {
        // Host-ignored: events sent plugin -> host are not in this host's set.
        return;
      }
      const respond = (args: unknown, error?: { code: string; message: string }) =>
        send({ id: env.id, type: "response", method: env.method, capability: env.capability ?? "", args, version: PROTOCOL_VERSION, error });

      switch (env.method) {
        case "perception.read":
          // The cockpit's boxes are produced locally on this (drone) node, so
          // report the local tier.
          respond({ tier: "local" });
          break;
        case "perception.health": {
          const latest = useDetectionsStore.getState().latest;
          const fresh =
            latest && Date.now() - latest.receivedAt <= 2000;
          const ageMs = latest ? Date.now() - latest.tsMs : null;
          respond({
            session: fresh ? "live" : "stalled",
            feed: fresh ? "fresh" : "stale",
            ageMs,
            batchesPerSecond: fresh ? 4 : 0,
            boundNode: null,
          });
          break;
        }
        case "perception.subscribe":
          respond({});
          break;
        case "perception.unsubscribe":
          respond({});
          break;
        case "telemetry.subscribe":
          // Acked so the plugin's subscribe promise resolves; this host does
          // not synthesise telemetry it does not own.
          respond({});
          break;
        default:
          // Not offered by this host — a structured refusal rather than a
          // hang, so the plugin's caller gets a prompt error.
          respond(null, { code: "not_available", message: `method ${env.method} not offered by the cockpit host` });
      }
    };

    window.addEventListener("message", handler);
    return () => {
      window.removeEventListener("message", handler);
      unsubscribe();
    };
  }, [srcDoc]);

  if (error) {
    return (
      <div className="flex h-full w-full items-center justify-center p-4 text-center text-[0.65rem] text-amber">
        Plugin surface unavailable: {error}
      </div>
    );
  }

  if (!srcDoc) {
    return (
      <div className="flex h-full w-full items-center justify-center text-[0.65rem] text-muted">
        Loading plugin surface…
      </div>
    );
  }

  return (
    <iframe
      ref={iframeRef}
      title={`plugin-${pluginId}`}
      sandbox="allow-scripts"
      srcDoc={srcDoc}
      className="h-full w-full border-0"
    />
  );
}
