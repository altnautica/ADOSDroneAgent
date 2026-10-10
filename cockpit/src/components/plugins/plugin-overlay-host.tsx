// One extension frame. Fetches the extension's GCS bundle from the agent
// (`GET /api/plugins/{id}/gcs/{entrypoint}`), runs it in the sandboxed frame
// document from `lib/plugin-frame`, and answers its bridge requests through
// `lib/plugin-host`, which enforces the granted capabilities. Host events
// (theme, the plugin's stored config) are posted only after the frame has
// loaded, so none are lost to a document that is not listening yet.

import { useEffect, useMemo, useRef, useState } from "react";

import type { InstalledExtension } from "@/lib/extensions";
import { buildPluginFrameHtml } from "@/lib/plugin-frame";
import { PROTOCOL_VERSION, createPluginHost, type RpcEnvelope } from "@/lib/plugin-host";
import { cn } from "@/lib/utils";
import { apiFetch, credentialHeaders } from "@/shared/api-fetch";
import { EXTENSION_THEMES, type ExtensionThemeId } from "@/shared/extension-theme.generated";

function currentTheme(): ExtensionThemeId {
  const t = typeof document !== "undefined" ? document.documentElement.dataset.theme : undefined;
  return t === "light" ? "brand-light" : t === "nvg" ? "nvg" : "brand-dark";
}

export function PluginFrameHost({
  ext,
  title,
  className,
}: {
  ext: InstalledExtension;
  title: string;
  className?: string;
}) {
  const [bundle, setBundle] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const entrypoint = ext.entrypoint;

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

    const onMessage = (ev: MessageEvent<RpcEnvelope>) => {
      if (ev.source !== iframe.contentWindow) return;
      const env = ev.data;
      if (!env || typeof env !== "object" || env.version !== PROTOCOL_VERSION) return;
      host.handle(env);
    };
    const onLoad = () => {
      post({
        type: "event",
        method: "theme.changed",
        capability: "theme.useTheme",
        args: EXTENSION_THEMES[currentTheme()],
        version: PROTOCOL_VERSION,
      });
      apiFetch<Record<string, unknown>>(`/api/plugins/${encodeURIComponent(ext.pluginId)}/config`)
        .then((values) => {
          if (!disposed && values && typeof values === "object") {
            post({ type: "event", method: "config.changed", capability: "", args: values, version: PROTOCOL_VERSION });
          }
        })
        .catch(() => undefined);
    };

    window.addEventListener("message", onMessage);
    iframe.addEventListener("load", onLoad);
    return () => {
      disposed = true;
      window.removeEventListener("message", onMessage);
      iframe.removeEventListener("load", onLoad);
      host.dispose();
    };
  }, [srcDoc, ext]);

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
