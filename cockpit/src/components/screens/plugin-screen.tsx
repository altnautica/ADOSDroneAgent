// The cockpit's plugin-contribution screen. Resolves the installed plugin that
// declares a GCS half (an iframe bundle) from `GET /api/plugins` and renders it
// inside the sandboxed bridge host. A node with no such plugin installed shows a
// clean "no plugin" panel rather than a broken iframe.

import { useEffect, useState } from "react";

import { PluginOverlayHost } from "@/components/plugins/plugin-overlay-host";
import { apiFetch } from "@/lib/api";

interface PluginSummary {
  plugin_id: string;
  gcs?: { entrypoint?: string; contributes?: unknown } | null;
}

export function PluginScreen() {
  const [target, setTarget] = useState<{
    pluginId: string;
    entrypoint: string;
  } | null>(null);
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    let cancelled = false;
    apiFetch<{ plugins?: PluginSummary[] }>("/api/plugins")
      .then((res) => {
        if (cancelled) return;
        const plugins = Array.isArray(res?.plugins) ? res.plugins : [];
        const first = plugins.find(
          (p) => p?.plugin_id && p?.gcs?.entrypoint,
        );
        if (first && first.gcs?.entrypoint) {
          setTarget({ pluginId: first.plugin_id, entrypoint: first.gcs.entrypoint });
        }
      })
      .catch(() => {
        // No plugin surface is not an error the operator must be told about on
        // the flying view; the panel simply stays empty.
      })
      .finally(() => {
        if (!cancelled) setLoaded(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (!loaded) {
    return (
      <div className="flex h-full w-full items-center justify-center text-[0.65rem] text-muted">
        Loading…
      </div>
    );
  }

  if (!target) {
    return (
      <div className="flex h-full w-full items-center justify-center p-4 text-center text-[0.65rem] text-muted">
        No plugin surface installed
      </div>
    );
  }

  return <PluginOverlayHost pluginId={target.pluginId} entrypoint={target.entrypoint} />;
}
