import { Plug, Upload } from "lucide-react";
import { ReadErrorCard } from "@/components/read-error-card";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ChangeEvent,
} from "react";
import { Trans, useTranslation } from "react-i18next";

import { PageShell } from "@/components/page-shell";
import { InstalledRow, type InstallEntry } from "@/components/plugins/installed-row";
import { PluginInstallDialog } from "@/components/plugins/install-dialog";
import { RegistryGrid } from "@/components/plugins/registry-grid";
import { UrlInstall } from "@/components/plugins/url-install";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { usePluginCatalog } from "@/hooks/use-plugin-catalog";
import { useResource } from "@/hooks/use-resource";
import { installName } from "@/lib/plugin-install-view";
import { apiFetch } from "@/shared/api-fetch";
import {
  parsePlugin,
  removePlugin,
  type PluginManifestSummary,
  type PluginSource,
} from "@/shared/api/plugin-install";
import { toast, toastFromError } from "@/shared/api/toast";
import { cn } from "@/lib/utils";

interface PluginsListResponse {
  installs: InstallEntry[];
}

export function ExtensionsRoute() {
  const { t } = useTranslation();
  const list = useResource<PluginsListResponse>("plugins", "/api/plugins", 8000);
  const catalog = usePluginCatalog();

  const installs = list.data?.installs ?? [];
  const installedVersions = new Map(installs.map((i) => [i.plugin_id, i.version] as const));
  const nameOf = (id: string) => installName(id, catalog.data?.plugins);

  const [confirm, setConfirm] = useState<{
    plugin: InstallEntry;
    action: "enable" | "disable";
  } | null>(null);
  const [removing, setRemoving] = useState<InstallEntry | null>(null);
  const [keepData, setKeepData] = useState(true);

  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [installSource, setInstallSource] = useState<PluginSource | null>(null);
  const [installManifest, setInstallManifest] =
    useState<PluginManifestSummary | null>(null);
  const [installOpen, setInstallOpen] = useState(false);
  const [parsing, setParsing] = useState(false);
  const [dragOver, setDragOver] = useState(false);

  async function applyAction(plugin: InstallEntry, action: "enable" | "disable") {
    const name = nameOf(plugin.plugin_id);
    try {
      await apiFetch(
        `/api/plugins/${encodeURIComponent(plugin.plugin_id)}/${action}`,
        { method: "POST" },
      );
      toast.ok(
        action === "enable"
          ? t("extensions.toast.enabled", { id: name })
          : t("extensions.toast.disabled", { id: name }),
      );
      list.refetch();
    } catch (err) {
      toastFromError(
        err,
        action === "enable" ? t("extensions.toast.enableFailed") : t("extensions.toast.disableFailed"),
      );
      throw err;
    }
  }

  async function applyRemove(plugin: InstallEntry) {
    try {
      await removePlugin(plugin.plugin_id, keepData);
      toast.ok(t("cockpit.extensions.removed", { id: nameOf(plugin.plugin_id) }));
      list.refetch();
    } catch (err) {
      toastFromError(err, t("cockpit.extensions.removeFailed"));
      throw err;
    }
  }

  // Both install paths — an uploaded archive and a catalog entry — preview the
  // manifest first and then run the same permission dialog.
  const beginInstall = useCallback(
    async (source: PluginSource) => {
      if (source.kind === "file" && !source.file.name.endsWith(".adosplug")) {
        toast.err(t("extensions.toast.badExtension"));
        return;
      }
      setParsing(true);
      try {
        const res = await parsePlugin(source);
        if (!res.ok) {
          toast.err(t("extensions.toast.parseFailedDetail", { detail: res.detail || res.kind }));
          return;
        }
        setInstallSource(source);
        setInstallManifest(res);
        setInstallOpen(true);
      } catch (err) {
        toastFromError(err, t("extensions.toast.parseFailed"));
      } finally {
        setParsing(false);
      }
    },
    [t],
  );

  function onPickFile() {
    fileInputRef.current?.click();
  }

  function onFileChosen(e: ChangeEvent<HTMLInputElement>) {
    const f = e.target.files?.[0];
    e.target.value = "";
    if (f) void beginInstall({ kind: "file", file: f });
  }

  // Window-level drag-drop. Lets the operator drop a .adosplug
  // anywhere on the page, not just onto the file picker.
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
      if (f) void beginInstall({ kind: "file", file: f });
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
      title={t("extensions.page.title")}
      blurb={t("extensions.page.blurb")}
      rightAction={
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={() => list.refetch()}>
            {t("extensions.page.refresh")}
          </Button>
          <Button size="sm" onClick={onPickFile} disabled={parsing}>
            <Upload className="h-3.5 w-3.5" />
            {parsing ? t("extensions.page.parsing") : t("extensions.page.install")}
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
        <p className="text-sm text-muted-foreground">{t("extensions.page.loading")}</p>
      )}

      {list.isError && (
        <ReadErrorCard message={t("extensions.page.listFailed")} onRetry={() => void list.refetch()} />
      )}

      {!list.isLoading && !list.isError && installs.length === 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 flex items-start gap-3">
            <Plug className="h-5 w-5 text-muted-foreground mt-0.5" />
            <div>
              <div className="text-sm font-medium">{t("extensions.empty.title")}</div>
              <div className="text-xs text-muted-foreground mt-1">
                <Trans
                  i18nKey="extensions.empty.body"
                  components={{
                    mono: <span className="font-mono" />,
                    strong: <span className="font-medium" />,
                  }}
                />
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      <div className="space-y-2">
        {installs.map((p) => (
          <InstalledRow
            key={p.plugin_id}
            install={p}
            name={nameOf(p.plugin_id)}
            onToggle={(action) => setConfirm({ plugin: p, action })}
            onRemove={() => {
              setKeepData(true);
              setRemoving(p);
            }}
          />
        ))}
      </div>

      <UrlInstall busy={parsing} onInstall={(source) => void beginInstall(source)} />

      <RegistryGrid
        installedVersions={installedVersions}
        busy={parsing}
        onInstall={(entry) =>
          void beginInstall({
            kind: "catalog",
            url: entry.download_url,
            sha256: entry.archive_sha256 ?? null,
          })
        }
      />

      <ConfirmDialog
        open={!!confirm}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title={
          confirm
            ? confirm.action === "enable"
              ? t("extensions.confirm.enableTitle", { id: nameOf(confirm.plugin.plugin_id) })
              : t("extensions.confirm.disableTitle", { id: nameOf(confirm.plugin.plugin_id) })
            : ""
        }
        description={
          confirm?.action === "disable"
            ? t("extensions.confirm.disableDescription")
            : t("extensions.confirm.enableDescription")
        }
        confirmLabel={
          confirm?.action === "enable" ? t("extensions.actions.enable") : t("extensions.actions.disable")
        }
        destructive={confirm?.action === "disable"}
        onConfirm={async () => {
          if (confirm) await applyAction(confirm.plugin, confirm.action);
        }}
      />

      <ConfirmDialog
        open={!!removing}
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        title={removing ? t("extensions.page.removeTitle", { name: nameOf(removing.plugin_id) }) : ""}
        description={
          <div className="space-y-3">
            <p>{t("extensions.page.removeBody")}</p>
            <div className="flex items-start gap-3">
              <Switch id="ext-keep-data" checked={keepData} onCheckedChange={setKeepData} />
              <div>
                <Label htmlFor="ext-keep-data">{t("cockpit.extensions.keepData")}</Label>
                <p className="text-xs">{t("cockpit.extensions.keepDataHint")}</p>
              </div>
            </div>
          </div>
        }
        confirmLabel={t("cockpit.extensions.remove")}
        destructive
        onConfirm={async () => {
          if (removing) await applyRemove(removing);
        }}
      />

      <PluginInstallDialog
        open={installOpen}
        source={installSource}
        manifest={installManifest}
        onOpenChange={(open) => {
          setInstallOpen(open);
          if (!open) {
            setInstallSource(null);
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
  const { t } = useTranslation();
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
          {t("extensions.page.dropToInstall")}
        </div>
      </div>
    </div>
  );
}

