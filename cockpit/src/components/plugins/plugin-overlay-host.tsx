// One extension frame. Fetches the extension's GCS bundle from the agent
// (`GET /api/plugins/{id}/gcs/{entrypoint}`), runs it in the sandboxed frame
// document from `lib/plugin-frame`, and answers its bridge requests through
// `lib/plugin-host`, which enforces the granted capabilities. Host events
// (theme, the plugin's stored config) are posted only after the frame has
// loaded, so none are lost to a document that is not listening yet; a theme
// switch while the frame is open is posted again. A mount may pass one
// `hostEvent` (the `video.overlay.props` host props): it is posted on load
// and again each time the mount hands in a new one.

import { useEffect, useMemo, useRef, useState } from "react";

import { followTheme, themeChangedEvent } from "@/lib/extension-theme";
import type { InstalledExtension } from "@/lib/extensions";
import { buildPluginFrameHtml } from "@/lib/plugin-frame";
import { PROTOCOL_VERSION, createPluginHost, type RpcEnvelope } from "@/lib/plugin-host";
import { cn } from "@/lib/utils";
import { apiFetch, credentialHeaders } from "@/shared/api-fetch";
import { useSettingsStore } from "@/stores/settings-store";

/** A host → frame event a mount keeps current. */
export interface HostEvent {
  method: string;
  /** The capability tag the event carries ("" when none). */
  capability?: string;
  args: unknown;
}

export function PluginFrameHost({
  ext,
  title,
  className,
  hostEvent = null,
}: {
  ext: InstalledExtension;
  title: string;
  className?: string;
  hostEvent?: HostEvent | null;
}) {
  const [bundle, setBundle] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const entrypoint = ext.entrypoint;
  // The frame's live poster once it has loaded, and the newest host event.
  const postRef = useRef<((env: RpcEnvelope) => void) | null>(null);
  const hostEventRef = useRef(hostEvent);

  useEffect(() => {
    if (!entrypoint) return;
    const ac = new AbortController();
    const path = entrypoint.split("/").map(encodeURIComponent).join("/");
    fetch(`/api/plugins/${encodeURIComponent(ext.pluginId)}/gcs/${path}`, {
      headers: credentialHeaders(),
      signal: ac.signal,
    })
      .then(async (res) => {
        if (!res.ok) throw new Error(`bundle ${res.status}`);
        setBundle(await res.text());
      })
      .catch((e: unknown) => {
        if (!ac.signal.aborted) setError(e instanceof Error ? e.message : String(e));
      });
    return () => ac.abort();
  }, [ext.pluginId, entrypoint]);

  const srcDoc = useMemo(() => (bundle ? buildPluginFrameHtml(bundle) : ""), [bundle]);

  useEffect(() => {
    const iframe = iframeRef.current;
    if (!srcDoc || !iframe) return;
    const post = (env: RpcEnvelope) => iframe.contentWindow?.postMessage(env, "*");
    const host = createPluginHost(ext, post);
    let disposed = false;
    let loaded = false;

    const onMessage = (ev: MessageEvent<RpcEnvelope>) => {
      if (ev.source !== iframe.contentWindow) return;
      const env = ev.data;
      if (!env || typeof env !== "object" || env.version !== PROTOCOL_VERSION) return;
      host.handle(env);
    };
    const onLoad = () => {
      loaded = true;
      postRef.current = post;
      post(themeChangedEvent(useSettingsStore.getState().theme));
      const he = hostEventRef.current;
      if (he) post({ type: "event", method: he.method, capability: he.capability ?? "", args: he.args, version: PROTOCOL_VERSION });
      apiFetch<Record<string, unknown>>(`/api/plugins/${encodeURIComponent(ext.pluginId)}/config`)
        .then((values) => {
          if (!disposed && values && typeof values === "object") {
            post({ type: "event", method: "config.changed", capability: "", args: values, version: PROTOCOL_VERSION });
          }
        })
        .catch(() => undefined);
    };

    const offTheme = followTheme(post, () => loaded);

    window.addEventListener("message", onMessage);
    iframe.addEventListener("load", onLoad);
    return () => {
      disposed = true;
      postRef.current = null;
      offTheme();
      window.removeEventListener("message", onMessage);
      iframe.removeEventListener("load", onLoad);
      host.dispose();
    };
  }, [srcDoc, ext]);

  useEffect(() => {
    hostEventRef.current = hostEvent;
    if (hostEvent) {
      postRef.current?.({
        type: "event",
        method: hostEvent.method,
        capability: hostEvent.capability ?? "",
        args: hostEvent.args,
        version: PROTOCOL_VERSION,
      });
    }
  }, [hostEvent]);

  if (!entrypoint || ext.isolation === "inline") {
    return (
      <div className={cn("flex items-center justify-center p-4 text-center text-[0.8rem] text-muted-foreground", className)}>
        {title} runs in Mission Control; this cockpit hosts sandboxed extension frames only.
      </div>
    );
  }
  if (error) {
    return (
      <div className={cn("flex items-center justify-center p-4 text-center text-[0.8rem] text-warn", className)}>
        {title} is unavailable: {error}
      </div>
    );
  }
  if (!srcDoc) {
    return (
      <div className={cn("flex items-center justify-center text-[0.8rem] text-muted-foreground", className)}>
        Loading {title}…
      </div>
    );
  }
  return (
    <iframe
      ref={iframeRef}
      title={title}
      sandbox="allow-scripts"
      srcDoc={srcDoc}
      className={cn("border-0 bg-transparent", className)}
    />
  );
}
