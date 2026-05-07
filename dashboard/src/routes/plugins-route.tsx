import { Plug, Upload } from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ChangeEvent,
} from "react";

import { PageShell } from "@/components/page-shell";
import { PluginInstallDialog } from "@/components/plugins/install-dialog";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
import {
  parsePlugin,
  type PluginErrorEnvelope,
  type PluginManifestSummary,
} from "@/lib/plugin-install";
import { toast, toastFromError } from "@/lib/toast";
import { cn } from "@/lib/utils";

interface InstallEntry {
  plugin_id: string;
  version?: string;
  status?: string;
  signer_id?: string | null;
  permissions?: Record<string, { granted: boolean }>;
}

interface PluginsListResponse {
  installs: InstallEntry[];
}

export function PluginsRoute() {
  const list = useResource<PluginsListResponse>("plugins", "/api/plugins", 8000);

  const installs = list.data?.installs ?? [];

  const [confirm, setConfirm] = useState<{
    plugin: InstallEntry;
    action: "enable" | "disable";
  } | null>(null);

  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [installFile, setInstallFile] = useState<File | null>(null);
  const [installManifest, setInstallManifest] =
    useState<PluginManifestSummary | null>(null);
  const [installOpen, setInstallOpen] = useState(false);
  const [parsing, setParsing] = useState(false);
  const [dragOver, setDragOver] = useState(false);

  async function applyAction(plugin: InstallEntry, action: "enable" | "disable") {
    try {
      await apiFetch(
        `/api/plugins/${encodeURIComponent(plugin.plugin_id)}/${action}`,
        { method: "POST" },
      );
      toast.ok(`${plugin.plugin_id} ${action}d.`);
      list.refetch();
    } catch (err) {
      toastFromError(err, `Plugin ${action} failed.`);
    }
  }

  const beginInstall = useCallback(
    async (file: File) => {
      if (!file.name.endsWith(".adosplug")) {
        toast.err("Plugin files must end in .adosplug");
        return;
      }
      setParsing(true);
      try {
        const res = (await parsePlugin(file)) as
          | PluginManifestSummary
          | PluginErrorEnvelope;
        if (!("ok" in res) || res.ok !== true) {
          const err = res as PluginErrorEnvelope;
          toast.err(`Plugin parse failed: ${err.detail || err.kind}`);
          return;
        }
        setInstallFile(file);
        setInstallManifest(res as PluginManifestSummary);
        setInstallOpen(true);
      } catch (err) {
        toastFromError(err, "Plugin parse failed.");
      } finally {
        setParsing(false);
      }
    },
    [],
  );

  function onPickFile() {
    fileInputRef.current?.click();
  }

  function onFileChosen(e: ChangeEvent<HTMLInputElement>) {
    const f = e.target.files?.[0];
    e.target.value = "";
    if (f) void beginInstall(f);
  }

  // Window-level drag-drop. Lets the operator drop a .adosplug
  // anywhere on the page (Foxglove pattern, spec section 1).
  useEffect(() => {
    const onDragOver = (e: globalThis.DragEvent) => {
      if (e.dataTransfer?.types.includes("Files")) {
        e.preventDefault();
        setDragOver(true);
      }
    };
    const onDragLeave = (e: globalThis.DragEvent) => {
      if (e.relatedTarget == null) setDragOver(false);
    };
    const onDrop = (e: globalThis.DragEvent) => {
      if (!e.dataTransfer?.types.includes("Files")) return;
      e.preventDefault();
      setDragOver(false);
      const f = e.dataTransfer.files[0];
      if (f) void beginInstall(f);
    };
    window.addEventListener("dragover", onDragOver);
    window.addEventListener("dragleave", onDragLeave);
    window.addEventListener("drop", onDrop);
    return () => {
      window.removeEventListener("dragover", onDragOver);
      window.removeEventListener("dragleave", onDragLeave);
      window.removeEventListener("drop", onDrop);
    };
  }, [beginInstall]);

  return (
    <PageShell
      title="Plugins"
      blurb="Installed extension manifests. Toggle plugins on or off without restarting the agent."
      rightAction={
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={() => list.refetch()}>
            Refresh
          </Button>
          <Button size="sm" onClick={onPickFile} disabled={parsing}>
            <Upload className="h-3.5 w-3.5" />
            {parsing ? "Parsing…" : "Install"}
          </Button>
          <input
            ref={fileInputRef}
            type="file"
            accept=".adosplug"
            className="hidden"
            onChange={onFileChosen}
          />
        </div>
      }
    >
      {list.isLoading && (
        <p className="text-sm text-muted-foreground">loading…</p>
      )}

      {!list.isLoading && installs.length === 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 flex items-start gap-3">
            <Plug className="h-5 w-5 text-muted-foreground mt-0.5" />
            <div>
              <div className="text-sm font-medium">No plugins installed.</div>
              <div className="text-xs text-muted-foreground mt-1">
                Drop a signed{" "}
                <span className="font-mono">.adosplug</span> file anywhere on
                this window, or click <span className="font-medium">Install</span>{" "}
                to pick one.
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      <div className="space-y-2">
        {installs.map((p) => {
          const enabled = p.status === "enabled" || p.status === "running";
          const grantedCount = p.permissions
            ? Object.values(p.permissions).filter((g) => g.granted).length
            : 0;
          const totalCount = p.permissions
            ? Object.keys(p.permissions).length
            : 0;
          return (
            <Card key={p.plugin_id}>
              <CardContent className="pt-4 pb-4 flex items-center justify-between gap-4">
                <div className="min-w-0">
                  <div className="flex items-center gap-2 flex-wrap">
                    <span className="font-mono text-sm">{p.plugin_id}</span>
                    {p.version && (
                      <span className="text-[10px] text-muted-foreground font-mono">
                        v{p.version}
                      </span>
                    )}
                    <span
                      className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${
                        enabled
                          ? "border-ok/40 text-ok"
                          : "border-muted-foreground/40 text-muted-foreground"
                      }`}
                    >
                      {p.status ?? (enabled ? "enabled" : "disabled")}
                    </span>
                    {p.signer_id ? (
                      <span className="text-[10px] uppercase tracking-wider text-muted-foreground">
                        signed
                      </span>
                    ) : (
                      <span className="text-[10px] uppercase tracking-wider text-destructive">
                        unsigned
                      </span>
                    )}
                  </div>
                  {totalCount > 0 && (
                    <p className="text-xs text-muted-foreground mt-1">
                      {grantedCount}/{totalCount} permissions granted
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
            ? `${confirm.action === "enable" ? "Enable" : "Disable"} ${confirm.plugin.plugin_id}?`
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

      <PluginInstallDialog
        open={installOpen}
        file={installFile}
        manifest={installManifest}
        onOpenChange={(open) => {
          setInstallOpen(open);
          if (!open) {
            setInstallFile(null);
            setInstallManifest(null);
          }
        }}
        onFinished={() => list.refetch()}
      />

      {dragOver && <DragDropOverlay />}
    </PageShell>
  );
}

function DragDropOverlay() {
  return (
    <div
      className={cn(
        "fixed inset-0 z-40 flex items-center justify-center pointer-events-none",
        "bg-background/70 backdrop-blur-sm",
      )}
      aria-hidden
    >
      <div className="rounded-lg border-2 border-dashed border-primary/60 bg-background p-8 shadow-xl">
        <Upload className="h-8 w-8 text-primary mx-auto mb-2" />
        <div className="text-sm font-medium text-center">
          Drop the .adosplug to install
        </div>
      </div>
    </div>
  );
}

