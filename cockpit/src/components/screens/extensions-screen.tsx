// Extensions: four tabs over the node's plugin host.
//   Running   — every running extension with its node tab (the
//               `node.detail.tab` frame) hosted inline.
//   Installed — every install (`GET /api/plugins`): enable, disable and remove,
//               the last two behind the guarded button.
//   Catalog   — the first-party catalog (`GET /api/v1/plugins/catalog`).
//   Install   — an `.adosplug` from a file or a URL.
// Every install previews the manifest first, then runs the permission review
// sheet, which installs on a slide.

import { useCallback, useEffect, useRef, useState, type ChangeEvent } from "react";
import { useTranslation } from "react-i18next";

import { InstallSheet, RiskBadge } from "@/components/extensions/install-sheet";
import { PluginFrameHost } from "@/components/plugins/plugin-overlay-host";
import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, EmptyNote, StaleBadge } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { SwitchSettingRow, TextSettingRow } from "@/components/ui/setting-row";
import { TwoPane } from "@/components/ui/two-pane";
import {
  catalogView,
  installState,
  isInstallUrl,
  normalizeSha256,
  parseInstalls,
  type CatalogEntry,
  type CatalogResponse,
  type InstallRow,
} from "@/lib/plugin-catalog";
import { cn } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import {
  disablePlugin,
  enablePlugin,
  parsePlugin,
  removePlugin,
  type PluginManifestSummary,
  type PluginSource,
} from "@/shared/api/plugin-install";
import { toast, toastFromError } from "@/shared/api/toast";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";
import { useExtensionsStore } from "@/stores/extensions-store";

const TAB_SLOT = "node.detail.tab";
const TAB_CAPABILITY = "ui.slot.node-detail-tab";
const LIST_POLL_MS = 8000;
const CATALOG_POLL_MS = 60_000;

export function ExtensionsScreen() {
  const { t } = useTranslation();
  const [tab, setTab] = useState(0);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [review, setReview] = useState<{ source: PluginSource; manifest: PluginManifestSummary } | null>(null);
  const [parsing, setParsing] = useState(false);
  const reloadRunning = useExtensionsStore((s) => s.load);

  const installs = useResource(
    useCallback((s: AbortSignal) => apiFetch<unknown>("/api/plugins", { signal: s }).then(parseInstalls), []),
    LIST_POLL_MS,
  );
  const catalog = useResource(
    useCallback((s: AbortSignal) => apiFetch<CatalogResponse>("/api/v1/plugins/catalog", { signal: s }), []),
    CATALOG_POLL_MS,
    { enabled: tab === 2 },
  );

  const refreshInstalls = installs.refresh;
  const changed = useCallback(() => {
    refreshInstalls();
    void reloadRunning();
  }, [refreshInstalls, reloadRunning]);

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
        setReview({ source, manifest: res });
      } catch (err) {
        toastFromError(err, t("extensions.toast.parseFailed"));
      } finally {
        setParsing(false);
      }
    },
    [t],
  );

  const rows = installs.data ?? [];
  const selected = rows.find((r) => r.pluginId === selectedId) ?? null;
  const installedVersions = new Map(rows.map((r) => [r.pluginId, r.version ?? undefined] as const));

  let body;
  if (tab === 0) {
    body = <RunningTab />;
  } else if (tab === 1) {
    body = !installs.ready ? (
      <EmptyNote>{t("extensions.page.loading")}</EmptyNote>
    ) : installs.error && !installs.data ? (
      <EmptyNote>{t("cockpit.extensions.unavailable", { error: installs.error })}</EmptyNote>
    ) : rows.length === 0 ? (
      <EmptyNote>{t("extensions.empty.title")}</EmptyNote>
    ) : (
      <TwoPane
        showDetailInPortrait={selected !== null}
        list={<InstallList rows={rows} selectedId={selectedId} onSelect={setSelectedId} />}
        detail={
          selected ? (
            <InstallDetail
              key={selected.pluginId}
              row={selected}
              onChanged={changed}
              onRemoved={() => {
                setSelectedId(null);
                changed();
              }}
            />
          ) : (
            <EmptyNote>{t("cockpit.extensions.pick")}</EmptyNote>
          )
        }
      />
    );
  } else if (tab === 2) {
    body = (
      <CatalogTab
        data={catalog.data}
        error={catalog.error}
        ready={catalog.ready}
        installed={installedVersions}
        busy={parsing}
        onInstall={(e) => void beginInstall({ kind: "catalog", url: e.download_url, sha256: e.archive_sha256 ?? null })}
      />
    );
  } else {
    body = <InstallTab busy={parsing} onSource={(s) => void beginInstall(s)} />;
  }

  return (
    <ConsoleScaffold
      title={t("extensions.page.title")}
      onBack={tab === 1 && selected ? () => setSelectedId(null) : undefined}
      backLabel={t("cockpit.extensions.back")}
      tabs={[
        t("cockpit.extensions.tabRunning"),
        t("cockpit.extensions.tabInstalled"),
        t("cockpit.extensions.tabCatalog"),
        t("cockpit.extensions.tabInstall"),
      ]}
      selectedTab={tab}
      onSelectTab={setTab}
      actions={<StaleBadge stale={installs.stale} />}
    >
      {body}
      {review ? (
        <InstallSheet
          source={review.source}
          manifest={review.manifest}
          onClose={() => setReview(null)}
          onFinished={changed}
        />
      ) : null}
    </ConsoleScaffold>
  );
}

function RunningTab() {
  const { t } = useTranslation();
  const extensions = useExtensionsStore((s) => s.extensions);
  const loaded = useExtensionsStore((s) => s.loaded);
  const error = useExtensionsStore((s) => s.error);
  const load = useExtensionsStore((s) => s.load);
  const [selected, setSelected] = useState<string | null>(null);

  useEffect(() => {
    void load();
  }, [load]);

  if (!loaded) return <EmptyNote>{t("extensions.page.loading")}</EmptyNote>;
  if (error) return <EmptyNote>{t("cockpit.extensions.unavailable", { error })}</EmptyNote>;
  if (extensions.length === 0) return <EmptyNote>{t("cockpit.extensions.noneRunning")}</EmptyNote>;

  const active = extensions.find((e) => e.pluginId === selected) ?? extensions[0];
  const frame = active.grantedCapabilities.includes(TAB_CAPABILITY)
    ? (active.panels.find((p) => p.slot === TAB_SLOT) ?? null)
    : null;
  return (
    <div className="flex h-full min-h-0 flex-col gap-[0.5rem]">
      <div className="flex flex-wrap gap-[0.35rem]" role="tablist" aria-label={t("extensions.page.title")}>
        {extensions.map((e) => (
          <button
            key={e.pluginId}
            type="button"
            role="tab"
            aria-selected={e === active}
            onClick={() => setSelected(e.pluginId)}
            className={cn(
              "touch-target rounded-md px-[0.8rem] text-[0.85rem]",
              e === active ? "bg-primary text-primary-foreground" : "bg-muted text-surface-foreground hover:bg-border",
            )}
          >
            {e.name}
          </button>
        ))}
      </div>
      <div className="text-[0.8rem] text-muted-foreground">
        {t("cockpit.extensions.surfaces", { skills: active.skills.length, panels: active.panels.length })}
      </div>
      {frame ? (
        <PluginFrameHost ext={active} title={frame.title} className="min-h-[16rem] w-full flex-1 rounded-lg" />
      ) : (
        <EmptyNote>{t("cockpit.extensions.noTab", { name: active.name })}</EmptyNote>
      )}
    </div>
  );
}

function InstallList({
  rows,
  selectedId,
  onSelect,
}: {
  rows: InstallRow[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-[0.2rem] pr-[0.4rem]">
      {rows.map((r) => (
        <ListItem
          key={r.pluginId}
          headline={r.pluginId}
          supporting={[
            r.version ? `v${r.version}` : null,
            r.status ?? (r.enabled ? t("extensions.status.enabled") : t("extensions.status.disabled")),
            r.signed ? t("extensions.status.signed") : t("extensions.status.unsigned"),
          ]
            .filter(Boolean)
            .join(" · ")}
          selected={r.pluginId === selectedId}
          onClick={() => onSelect(r.pluginId)}
        />
      ))}
    </div>
  );
}

function InstallDetail({
  row,
  onChanged,
  onRemoved,
}: {
  row: InstallRow;
  onChanged: () => void;
  onRemoved: () => void;
}) {
  const { t } = useTranslation();
  const [keepData, setKeepData] = useState(false);
  const [busy, setBusy] = useState(false);

  async function run(action: () => Promise<unknown>, ok: string, fail: string, after: () => void) {
    setBusy(true);
    try {
      await action();
      toast.ok(ok);
      after();
    } catch (err) {
      toastFromError(err, fail);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-[0.5rem] pl-[0.6rem]">
      <div className="font-mono text-[0.85rem]">{row.pluginId}</div>
      <div className="text-[0.8rem] text-muted-foreground">
        {[row.version ? `v${row.version}` : null, row.status, row.signed ? t("extensions.status.signed") : t("extensions.status.unsigned")]
          .filter(Boolean)
          .join(" · ")}
      </div>
      {row.declared > 0 ? (
        <div className="text-[0.8rem] text-muted-foreground">
          {t("extensions.status.permissionsGranted", { granted: row.granted, total: row.declared })}
        </div>
      ) : null}
      {row.enabled ? (
        <GuardedButton
          label={t("extensions.actions.disable")}
          guardLabel={t("cockpit.extensions.disableGuard")}
          disabled={busy}
          onFired={() =>
            void run(
              () => disablePlugin(row.pluginId),
              t("extensions.toast.disabled", { id: row.pluginId }),
              t("extensions.toast.disableFailed"),
              onChanged,
            )
          }
        />
      ) : (
        <ActionButton
          label={t("extensions.actions.enable")}
          variant="primary"
          busy={busy}
          onClick={() =>
            void run(
              () => enablePlugin(row.pluginId),
              t("extensions.toast.enabled", { id: row.pluginId }),
              t("extensions.toast.enableFailed"),
              onChanged,
            )
          }
        />
      )}
      <SwitchSettingRow
        title={t("cockpit.extensions.keepData")}
        summary={t("cockpit.extensions.keepDataHint")}
        checked={keepData}
        onChange={setKeepData}
      />
      <GuardedButton
        label={t("cockpit.extensions.remove")}
        guardLabel={t("cockpit.extensions.removeGuard")}
        disabled={busy}
        onFired={() =>
          void run(
            () => removePlugin(row.pluginId, keepData),
            t("cockpit.extensions.removed", { id: row.pluginId }),
            t("cockpit.extensions.removeFailed"),
            onRemoved,
          )
        }
      />
    </div>
  );
}

function CatalogTab({
  data,
  error,
  ready,
  installed,
  busy,
  onInstall,
}: {
  data: CatalogResponse | null;
  error: string | null;
  ready: boolean;
  installed: ReadonlyMap<string, string | undefined>;
  busy: boolean;
  onInstall: (entry: CatalogEntry) => void;
}) {
  const { t } = useTranslation();
  const view = catalogView(data, error, ready);
  if (view.kind === "loading") return <EmptyNote>{t("extensions.page.loading")}</EmptyNote>;
  if (view.kind === "error") return <EmptyNote>{t("extensions.catalog.loadFailed", { message: view.message })}</EmptyNote>;
  if (view.kind === "parseError") return <EmptyNote>{t("extensions.catalog.parseFailed", { message: view.message })}</EmptyNote>;
  if (view.kind === "empty") return <EmptyNote>{t("cockpit.extensions.catalogEmpty")}</EmptyNote>;
  return (
    <div className="flex flex-col gap-[0.4rem]">
      <p className="text-[0.8rem] text-muted-foreground">{t("extensions.catalog.subtitle")}</p>
      {view.parseError ? (
        <p className="text-[0.8rem] text-warn">{t("extensions.catalog.parseFailed", { message: view.parseError })}</p>
      ) : null}
      {view.entries.map((e) => {
        const st = installState(e, installed);
        return (
          <ListItem
            key={e.id}
            headline={`${e.name} · v${e.version}`}
            supporting={e.description}
            trailing={
              <div className="flex items-center gap-[0.4rem]">
                {e.risk ? <RiskBadge level={e.risk} /> : null}
                {st === "installed" ? (
                  <span className="text-[0.75rem] text-ok">{t("extensions.catalog.installed")}</span>
                ) : (
                  <ActionButton
                    label={st === "update" ? t("extensions.catalog.updateTo", { version: e.version }) : t("extensions.page.install")}
                    variant="primary"
                    disabled={busy}
                    onClick={() => onInstall(e)}
                  />
                )}
              </div>
            }
          />
        );
      })}
    </div>
  );
}

function InstallTab({ busy, onSource }: { busy: boolean; onSource: (s: PluginSource) => void }) {
  const { t } = useTranslation();
  const fileRef = useRef<HTMLInputElement | null>(null);
  const [url, setUrl] = useState("");
  const [sha, setSha] = useState("");
  const pin = normalizeSha256(sha);
  const urlOk = isInstallUrl(url);

  function onFile(e: ChangeEvent<HTMLInputElement>) {
    const f = e.target.files?.[0];
    e.target.value = "";
    if (f) onSource({ kind: "file", file: f });
  }

  return (
    <div
      className="flex max-w-[40rem] flex-col gap-[0.6rem]"
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes("Files")) e.preventDefault();
      }}
      onDrop={(e) => {
        const f = e.dataTransfer.files[0];
        if (!f) return;
        e.preventDefault();
        onSource({ kind: "file", file: f });
      }}
    >
      <p className="text-[0.85rem] text-muted-foreground">{t("cockpit.extensions.installHint")}</p>
      <ActionButton
        label={busy ? t("extensions.page.parsing") : t("cockpit.extensions.pickFile")}
        variant="primary"
        busy={busy}
        onClick={() => fileRef.current?.click()}
      />
      <input ref={fileRef} type="file" accept=".adosplug" className="hidden" onChange={onFile} />
      <TextSettingRow
        title={t("cockpit.extensions.url")}
        value={url}
        onChange={setUrl}
        inputMode="url"
        placeholder={t("cockpit.extensions.urlPlaceholder")}
      />
      <TextSettingRow
        title={t("cockpit.extensions.sha256")}
        summary={pin === "invalid" ? t("cockpit.extensions.sha256Invalid") : t("cockpit.extensions.sha256Hint")}
        value={sha}
        onChange={setSha}
      />
      <ActionButton
        label={t("cockpit.extensions.fetchUrl")}
        disabled={busy || !urlOk || pin === "invalid"}
        onClick={() => onSource({ kind: "url", url: url.trim(), sha256: pin === "invalid" ? null : pin })}
      />
    </div>
  );
}
