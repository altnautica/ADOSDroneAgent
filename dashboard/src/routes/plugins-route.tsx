import { Plug } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { ApiError, apiFetch } from "@/lib/api";

interface PluginEntry {
  id: string;
  name?: string;
  version?: string;
  state?: string;
  enabled?: boolean;
  manifest?: { name?: string; version?: string; description?: string };
}

interface PluginsResponse {
  plugins: PluginEntry[];
}

export function PluginsRoute() {
  const list = useResource<PluginsResponse | PluginEntry[]>(
    "plugins",
    "/api/plugins",
    8000,
  );

  const plugins: PluginEntry[] = Array.isArray(list.data)
    ? list.data
    : (list.data?.plugins ?? []);

  const [confirm, setConfirm] = useState<{
    plugin: PluginEntry;
    action: "enable" | "disable";
  } | null>(null);
  const [feedback, setFeedback] = useState<{ kind: "ok" | "err"; text: string } | null>(
    null,
  );

  async function applyAction(plugin: PluginEntry, action: "enable" | "disable") {
    setFeedback(null);
    try {
      await apiFetch(`/api/plugins/${encodeURIComponent(plugin.id)}/${action}`, {
        method: "POST",
      });
      setFeedback({
        kind: "ok",
        text: `${plugin.id} ${action}d.`,
      });
      list.refetch();
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    }
  }

  return (
    <PageShell
      title="Plugins"
      blurb="Installed extension manifests. Toggle plugins on or off without restarting the agent."
      rightAction={
        <Button variant="outline" size="sm" onClick={() => list.refetch()}>
          Refresh
        </Button>
      }
    >
      {list.isLoading && (
        <p className="text-sm text-muted-foreground">loading…</p>
      )}

      {feedback && (
        <div
          className={`rounded-md border px-3 py-2 text-sm ${
            feedback.kind === "ok"
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300"
          }`}
        >
          {feedback.text}
        </div>
      )}

      {!list.isLoading && plugins.length === 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 flex items-start gap-3">
            <Plug className="h-5 w-5 text-muted-foreground mt-0.5" />
            <div>
              <div className="text-sm font-medium">No plugins installed.</div>
              <div className="text-xs text-muted-foreground mt-1">
                Install a signed{" "}
                <span className="font-mono">.adosplug</span> package via{" "}
                <span className="font-mono">ados plugin install</span> on the
                agent. The webapp install flow lands in a follow-up release.
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      <div className="space-y-2">
        {plugins.map((p) => {
          const enabled =
            p.enabled ?? (p.state === "enabled" || p.state === "running");
          return (
            <Card key={p.id}>
              <CardContent className="pt-4 pb-4 flex items-center justify-between gap-4">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="font-mono text-sm">{p.id}</span>
                    {p.version && (
                      <span className="text-[10px] text-muted-foreground font-mono">
                        v{p.version}
                      </span>
                    )}
                    <span
                      className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${
                        enabled
                          ? "border-emerald-500/40 text-emerald-500"
                          : "border-muted-foreground/40 text-muted-foreground"
                      }`}
                    >
                      {p.state ?? (enabled ? "enabled" : "disabled")}
                    </span>
                  </div>
                  {p.manifest?.description && (
                    <p className="text-xs text-muted-foreground mt-1 truncate">
                      {p.manifest.description}
                    </p>
                  )}
                </div>
                <div className="shrink-0 flex items-center gap-2">
                  <Button
                    size="sm"
                    variant={enabled ? "outline" : "default"}
                    onClick={() =>
                      setConfirm({
                        plugin: p,
                        action: enabled ? "disable" : "enable",
                      })
                    }
                  >
                    {enabled ? "Disable" : "Enable"}
                  </Button>
                </div>
              </CardContent>
            </Card>
          );
        })}
      </div>

      <ConfirmDialog
        open={!!confirm}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title={
          confirm
            ? `${confirm.action === "enable" ? "Enable" : "Disable"} ${confirm.plugin.id}?`
            : ""
        }
        description={
          confirm?.action === "disable"
            ? "The plugin will stop processing immediately. Any background work is killed."
            : "The plugin will start with its declared capabilities."
        }
        confirmLabel={confirm?.action === "enable" ? "Enable" : "Disable"}
        destructive={confirm?.action === "disable"}
        onConfirm={async () => {
          if (confirm) await applyAction(confirm.plugin, confirm.action);
        }}
      />
    </PageShell>
  );
}
