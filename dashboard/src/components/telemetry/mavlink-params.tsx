import { useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Radio, RotateCcw, Save, Search } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { BitmaskModal } from "@/components/fc/bitmask-modal";
import { EnumSelect } from "@/components/fc/enum-select";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useResource } from "@/hooks/use-resource";
import { useSnapshot } from "@/hooks/use-snapshot";
import { ApiError, apiFetch } from "@/shared/api-fetch";
import { resolveFirmwareType } from "@/lib/fc-firmware";
import { loadParamMetadata } from "@/lib/param-metadata";
import {
  buildRows,
  paramWriteBlock,
  categoryCounts,
  filterRows,
  type FilterState,
  type ParamRow,
} from "@/lib/params";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { useParamsStore } from "@/stores/params-store";

import { displayValue, type ParamSetResponse, type ParamsResponse } from "./param-display";

export function MavlinkParams() {
  const { t } = useTranslation();
  const params = useResource<ParamsResponse>("params", "/api/params", 10000);
  const { drafts, setDraft, clearAll } = useParamsStore();

  // FC identity → the offline metadata catalog (enum labels, ranges, units).
  const hb = useHeartbeat();
  const snap = useSnapshot();
  const firmwareType = resolveFirmwareType(hb.data?.fcFirmware, snap.data?.fc?.vehicle);
  const meta = useQuery({
    queryKey: ["param-metadata", firmwareType],
    queryFn: () => loadParamMetadata({ firmwareType }),
    enabled: firmwareType !== "unknown",
    staleTime: Infinity,
  }).data;

  // Writes go out only while the FC itself reports disarmed. `null` is the
  // agent saying the heartbeat has not reported it yet, which is not disarmed.
  const writeBlock = paramWriteBlock(snap.data?.fc?.armed);
  const isPx4 = firmwareType === "px4";

  const [search, setSearch] = useState("");
  const [debouncedSearch, setDebouncedSearch] = useState("");
  const [category, setCategory] = useState<string | null>(null);
  const [modifiedOnly, setModifiedOnly] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [bitmaskTarget, setBitmaskTarget] = useState<{
    name: string;
    original: number;
    current: number;
  } | null>(null);
  const [saveProgress, setSaveProgress] = useState<{
    total: number;
    done: number;
  } | null>(null);

  // Debounce the search input so typing doesn't re-filter the whole
  // list on every keystroke (param tables can run 800+ rows).
  useEffect(() => {
    const timer = setTimeout(() => setDebouncedSearch(search), 150);
    return () => clearTimeout(timer);
  }, [search]);

  const rows: ParamRow[] = useMemo(
    () => buildRows(params.data?.params ?? {}),
    [params.data],
  );

  const counts = useMemo(() => categoryCounts(rows), [rows]);

  const modifiedSet = useMemo(() => new Set(drafts.keys()), [drafts]);

  const filter: FilterState = {
    category,
    search: debouncedSearch,
    modifiedOnly,
    modified: modifiedSet,
  };

  const visibleRows = useMemo(
    () => filterRows(rows, filter),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [rows, category, debouncedSearch, modifiedOnly, modifiedSet],
  );

  const parentRef = useRef<HTMLDivElement | null>(null);
  const rowVirtualizer = useVirtualizer({
    count: visibleRows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 40,
    overscan: 12,
  });

  const dirtyCount = drafts.size;
  useDirtyGuard(dirtyCount > 0);

  async function saveAll() {
    const items = Array.from(drafts.entries());
    setSaveProgress({ total: items.length, done: 0 });
    let okCount = 0;
    let failCount = 0;
    let pendingCount = 0;

    for (let i = 0; i < items.length; i++) {
      const [name, draft] = items[i];
      try {
        const res = await apiFetch<ParamSetResponse>(
          `/api/params/${encodeURIComponent(name)}`,
          {
            method: "POST",
            body: { value: draft.draft },
          },
        );
        if (res.ack) {
          okCount++;
          // Drop the draft once acked
          useParamsStore.getState().discardDraft(name);
        } else {
          pendingCount++;
        }
      } catch (err) {
        failCount++;
        const msg =
          err instanceof ApiError ? err.message : err instanceof Error ? err.message : "";
        toast.err(t("telemetry.params.saveItemFailed", { name }), msg);
      }
      setSaveProgress({ total: items.length, done: i + 1 });
    }

    setSaveProgress(null);
    params.refetch();
    if (failCount === 0 && pendingCount === 0) {
      toast.ok(t("telemetry.params.savedCount", { count: okCount }));
    } else {
      toast.info(
        t("telemetry.params.savedPartial", { ok: okCount, failed: failCount, pending: pendingCount }),
        t("telemetry.params.savedPartialHint"),
      );
    }
  }

  return (
    <div className="grid grid-cols-1 md:grid-cols-[180px_1fr] gap-4">
      {/* Category sidebar */}
      <aside className="space-y-1">
        <button
          type="button"
          onClick={() => setCategory(null)}
          className={cn(
            "w-full text-left px-2 py-1.5 rounded text-sm flex items-center justify-between",
            category === null
              ? "bg-accent text-accent-foreground"
              : "text-muted-foreground hover:bg-accent/40",
          )}
        >
          <span className="font-mono">{t("telemetry.params.all")}</span>
          <span className="text-[10px] tabular-nums">{rows.length}</span>
        </button>
        {Object.entries(counts)
          .sort()
          .map(([cat, n]) => (
            <button
              key={cat}
              type="button"
              onClick={() => setCategory(cat)}
              className={cn(
                "w-full text-left px-2 py-1 rounded text-xs flex items-center justify-between",
                category === cat
                  ? "bg-accent text-accent-foreground"
                  : "text-muted-foreground hover:bg-accent/40",
              )}
            >
              <span className="font-mono">{cat}</span>
              <span className="text-[10px] tabular-nums">{n}</span>
            </button>
          ))}
      </aside>

      {/* Main grid */}
      <div className="min-w-0 space-y-3">
        <div className="flex flex-col sm:flex-row gap-3 items-start sm:items-center">
          <div className="relative flex-1 min-w-0">
            <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-muted-foreground" />
            <Input
              placeholder={t("telemetry.params.searchPlaceholder")}
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              className="pl-8"
            />
          </div>
          <label className="flex items-center gap-2 text-xs text-muted-foreground whitespace-nowrap cursor-pointer">
            <input
              type="checkbox"
              checked={modifiedOnly}
              onChange={(e) => setModifiedOnly(e.target.checked)}
              className="accent-primary"
            />
            {t("telemetry.params.modifiedOnly")}
          </label>
          <Button
            size="sm"
            variant="outline"
            disabled={dirtyCount === 0 || !!saveProgress}
            onClick={() => clearAll()}
          >
            <RotateCcw className="h-3.5 w-3.5" />
            {t("telemetry.params.discard")}
          </Button>
          <Button
            size="sm"
            disabled={dirtyCount === 0 || !!saveProgress || writeBlock !== null}
            onClick={() => setConfirmOpen(true)}
          >
            <Save className="h-3.5 w-3.5" />
            {saveProgress
              ? `${saveProgress.done}/${saveProgress.total}…`
              : t("telemetry.params.saveButton", { n: dirtyCount || "" })}
          </Button>
        </div>

        {writeBlock && <p className="text-xs text-warn">{writeBlock}</p>}

        {params.isLoading && (
          <p className="text-sm text-muted-foreground">{t("telemetry.params.loading")}</p>
        )}

        {params.isError && (
          <Card>
            <CardContent className="pt-5 pb-5 flex items-center justify-between gap-3">
              <p className="text-sm text-destructive">
                {t("telemetry.params.serviceUnreachable")}
              </p>
              <Button
                size="sm"
                variant="outline"
                onClick={() => params.refetch?.()}
              >
                {t("common.retry")}
              </Button>
            </CardContent>
          </Card>
        )}

        {params.data?.priming && rows.length === 0 && (
          <Card>
            <CardContent className="pt-5 pb-5 flex items-start gap-3">
              <Radio className="h-5 w-5 text-accent-primary mt-0.5 animate-pulse" />
              <div className="flex-1 space-y-2">
                <div className="text-sm font-medium">
                  {t("telemetry.params.priming")}
                </div>
                <div className="text-xs text-muted-foreground">
                  {t("telemetry.params.primingProgress", {
                    got: params.data.progress?.got ?? 0,
                    expected: params.data.progress?.expected ?? "?",
                  })}
                </div>
                {params.data.progress?.expected ? (
                  <div className="h-1 w-full overflow-hidden rounded bg-muted">
                    <div
                      className="h-full bg-accent-primary transition-[width]"
                      style={{
                        width: `${Math.min(
                          100,
                          Math.round(
                            ((params.data.progress.got ?? 0) /
                              params.data.progress.expected) *
                              100,
                          ),
                        )}%`,
                      }}
                    />
                  </div>
                ) : null}
              </div>
            </CardContent>
          </Card>
        )}

        {!params.isLoading &&
          !params.data?.priming &&
          rows.length === 0 &&
          (params.data?.priming_timeout || params.data?.priming_send_failed) && (
            <Card>
              <CardContent className="pt-5 pb-5 flex items-start gap-3">
                <Radio className="h-5 w-5 text-warn mt-0.5" />
                <div className="space-y-1">
                  <div className="text-sm font-medium">
                    {t("telemetry.params.fcUnreachable")}
                  </div>
                  <div className="text-xs text-muted-foreground">
                    {params.data?.priming_send_failed
                      ? t("telemetry.params.primingSendFailed")
                      : t("telemetry.params.primingTimeout")}
                  </div>
                </div>
              </CardContent>
            </Card>
          )}

        {!params.isLoading &&
          !params.data?.priming &&
          !params.data?.priming_timeout &&
          !params.data?.priming_send_failed &&
          rows.length === 0 && (
            <Card>
              <CardContent className="pt-5 pb-5 flex items-start gap-3">
                <Radio className="h-5 w-5 text-muted-foreground mt-0.5" />
                <div className="space-y-1">
                  <div className="text-sm font-medium">
                    {t("telemetry.params.emptyTitle")}
                  </div>
                  <div className="text-xs text-muted-foreground">
                    {t("telemetry.params.emptyBody")}
                  </div>
                </div>
              </CardContent>
            </Card>
          )}

        {visibleRows.length > 0 && (
          <Card className="p-0">
            <div
              ref={parentRef}
              className="overflow-y-auto rounded-md"
              style={{ height: "65vh" }}
            >
              <div
                style={{
                  height: rowVirtualizer.getTotalSize(),
                  width: "100%",
                  position: "relative",
                }}
              >
                {rowVirtualizer.getVirtualItems().map((vi) => {
                  const row = visibleRows[vi.index];
                  const draft = drafts.get(row.name);
                  const dirty = !!draft;
                  const m = meta?.get(row.name);
                  const hint = m?.range
                    ? `${m.range.min}…${m.range.max}${m.units ? ` ${m.units}` : ""}`
                    : (m?.units ?? "");
                  return (
                    <div
                      key={row.name}
                      style={{
                        position: "absolute",
                        top: 0,
                        left: 0,
                        width: "100%",
                        height: vi.size,
                        transform: `translateY(${vi.start}px)`,
                      }}
                      className={cn(
                        "flex items-center gap-3 px-3 border-b border-border/50 text-xs font-mono",
                        dirty && "bg-warn/5",
                      )}
                    >
                      <span className="text-muted-foreground w-14 shrink-0">
                        {row.category}
                      </span>
                      <span
                        className="flex-1 truncate"
                        title={m?.humanName || m?.description || row.name}
                      >
                        {row.name}
                      </span>
                      <span className="text-muted-foreground w-40 text-right truncate tabular-nums" title={displayValue(row.value, m)}>
                        {displayValue(row.value, m)}
                      </span>
                      {m?.values && m.values.size > 0 ? (
                        <EnumSelect
                          values={m.values}
                          value={draft ? draft.draft : row.value}
                          onChange={(v) => setDraft(row.name, row.value, v)}
                          className="w-28"
                        />
                      ) : m?.bitmask && m.bitmask.size > 0 ? (
                        <Button
                          size="sm"
                          variant="outline"
                          className="w-28 h-7 justify-start text-xs font-mono"
                          onClick={() =>
                            setBitmaskTarget({
                              name: row.name,
                              original: row.value,
                              current: draft ? draft.draft : row.value,
                            })
                          }
                        >
                          {t("telemetry.params.setBits")}
                        </Button>
                      ) : (
                        <Input
                          type="number"
                          step="any"
                          value={draft ? draft.draft : row.value}
                          onChange={(e) => {
                            const v = parseFloat(e.target.value);
                            if (!Number.isFinite(v)) return;
                            setDraft(row.name, row.value, v);
                          }}
                          className="w-28 h-7 text-xs"
                        />
                      )}
                      <span className="text-muted-foreground/70 w-24 shrink-0 truncate text-[10px]" title={hint}>
                        {hint}
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          </Card>
        )}

        {visibleRows.length === 0 && rows.length > 0 && (
          <p className="text-sm text-muted-foreground">
            {t("telemetry.params.noMatch")}
          </p>
        )}
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={t("telemetry.params.confirmTitle", { count: dirtyCount })}
        description={
          <div className="space-y-2">
            <p>
              {t("telemetry.params.confirmBody")}{" "}
              {isPx4 ? t("telemetry.params.confirmPx4") : t("telemetry.params.confirmArdupilot")}
            </p>
            <p className="text-xs text-muted-foreground">
              {t("telemetry.params.confirmWarning")}
            </p>
          </div>
        }
        confirmLabel={t("telemetry.params.saveAll")}
        destructive
        onConfirm={saveAll}
      />

      {bitmaskTarget &&
        (() => {
          const bm = meta?.get(bitmaskTarget.name)?.bitmask;
          if (!bm) return null;
          return (
            <BitmaskModal
              open
              onOpenChange={(o) => {
                if (!o) setBitmaskTarget(null);
              }}
              name={bitmaskTarget.name}
              bitmask={bm}
              value={bitmaskTarget.current}
              onApply={(next) =>
                setDraft(bitmaskTarget.name, bitmaskTarget.original, next)
              }
            />
          );
        })()}
    </div>
  );
}
