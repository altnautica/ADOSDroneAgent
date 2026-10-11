import { useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Radio, RotateCcw, Save, Search } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import i18n from "@/i18n";

import { BitmaskModal } from "@/components/fc/bitmask-modal";
import { EnumSelect } from "@/components/fc/enum-select";
import { MspSensorsView } from "@/components/fc/msp-sensors-view";
import { MspSettingsTab } from "@/components/fc/msp-settings-tab";
import { PageShell } from "@/components/page-shell";
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
import { fmtNum, fmtPct, fmtVoltage } from "@/shared/format";
import { loadParamMetadata, type ParamMetadata } from "@/lib/param-metadata";
import {
  buildRows,
  paramWriteBlock,
  categoryCounts,
  filterRows,
  formatParamValue,
  type FilterState,
  type ParamRow,
} from "@/lib/params";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { useParamsStore } from "@/stores/params-store";

/** Current value decorated with its enum label or set bitmask flags, when
 *  the metadata names them. */
function displayValue(value: number, m?: ParamMetadata): string {
  if (m?.values) {
    const label = m.values.get(value);
    if (label) return `${formatParamValue(value)} · ${label}`;
  }
  if (m?.bitmask && m.bitmask.size > 0) {
    const set = [...m.bitmask.entries()]
      .filter(([bit]) => (value & (1 << bit)) !== 0)
      .map(([, label]) => label);
    return `${formatParamValue(value)} · ${set.length ? set.join(", ") : i18n.t("telemetry.params.bitsNone")}`;
  }
  return formatParamValue(value);
}

interface ParamsResponse {
  params: Record<string, number>;
  count: number;
  cached: number;
  priming?: boolean;
  priming_timeout?: boolean;
  priming_send_failed?: boolean;
  progress?: { got: number; expected: number };
}

interface ParamSetResponse {
  name: string;
  value: number;
  ack: boolean;
  cached_value: number | null;
  message: string;
}

export function TelemetryRoute() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<"parameters" | "sensors">("parameters");

  return (
    <PageShell
      title={t("telemetry.page.title")}
      blurb={t("telemetry.page.blurb")}
      maxWidth="max-w-6xl"
      rightAction={
        <div className="flex items-center gap-1 rounded-md border border-border p-0.5 text-xs">
          <button
            type="button"
            onClick={() => setTab("parameters")}
            className={cn(
              "px-3 py-1 rounded transition-colors",
              tab === "parameters"
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:bg-accent/40",
            )}
          >
            {t("telemetry.tabs.parameters")}
          </button>
          <button
            type="button"
            onClick={() => setTab("sensors")}
            className={cn(
              "px-3 py-1 rounded transition-colors",
              tab === "sensors"
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:bg-accent/40",
            )}
          >
            {t("telemetry.tabs.sensors")}
          </button>
        </div>
      }
    >
      {tab === "parameters" ? <ParametersTab /> : <SensorsTab />}
    </PageShell>
  );
}

function ParametersTab() {
  // Route by FC family. Betaflight and iNav have no `/api/params` cache (the
  // agent is a byte-pipe for MSP), so they read settings from the FC directly
  // over the transparent MSP proxy; ArduPilot / PX4 use the agent's parameter
  // cache. The dispatch component reads only the identity so its hooks stay
  // unconditional.
  const hb = useHeartbeat();
  const snap = useSnapshot();
  const firmwareType = resolveFirmwareType(hb.data?.fcFirmware, snap.data?.fc?.vehicle);
  if (firmwareType === "betaflight" || firmwareType === "inav") {
    return (
      <MspSettingsTab
        firmware={firmwareType}
        firmwareVersion={snap.data?.fc?.firmware ?? undefined}
      />
    );
  }
  return <MavlinkParams />;
}

function MavlinkParams() {
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

function SensorsTab() {
  // Route by FC family. A Betaflight/iNav FC speaks MSP: the agent decodes no
  // MSP telemetry (it is a byte-pipe), so the browser polls the FC directly over
  // the transparent proxy for live attitude/battery/GPS/RC/sensors. ArduPilot /
  // PX4 sensors come from the agent's MAVLink vehicle-state snapshot.
  const hb = useHeartbeat();
  const snap = useSnapshot();
  const firmwareType = resolveFirmwareType(hb.data?.fcFirmware, snap.data?.fc?.vehicle);
  if (firmwareType === "betaflight" || firmwareType === "inav") {
    return <MspSensorsView firmware={firmwareType} />;
  }
  return <MavlinkSensors />;
}

// GET /api/telemetry — the router's live vehicle snapshot (attitude in
// radians, battery, GPS, position). Every field is absent or null until the FC
// reports it; nothing here is defaulted.
interface VehicleTelemetry {
  armed?: boolean | null;
  mode?: string | null;
  attitude?: { roll?: number | null; pitch?: number | null; yaw?: number | null };
  battery?: { voltage?: number | null; current?: number | null; remaining?: number | null };
  gps?: { fix_type?: number | null; satellites?: number | null; eph?: number | null };
  position?: { lat?: number | null; lon?: number | null; alt_rel?: number | null };
}

function SensorField({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between border-b border-border/50 py-1.5 last:border-b-0">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="font-mono text-sm">{value}</span>
    </div>
  );
}

function MavlinkSensors() {
  const { t } = useTranslation();
  const tel = useResource<VehicleTelemetry>("vehicle-telemetry", "/api/telemetry", 1_000);
  const v = tel.data;
  const deg = (rad: number | null | undefined) =>
    rad == null ? "—" : `${fmtNum((rad * 180) / Math.PI, 1)}°`;
  const hasPosition = v?.position?.lat != null && v?.position?.lon != null;

  if (tel.isError && !v) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-destructive">
          {t("telemetry.sensors.readFailed")}
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">{t("telemetry.sensors.attitude")}</div>
          <SensorField label={t("telemetry.sensors.roll")} value={deg(v?.attitude?.roll)} />
          <SensorField label={t("telemetry.sensors.pitch")} value={deg(v?.attitude?.pitch)} />
          <SensorField label={t("telemetry.sensors.yaw")} value={deg(v?.attitude?.yaw)} />
        </CardContent>
      </Card>
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">{t("telemetry.sensors.battery")}</div>
          <SensorField label={t("telemetry.sensors.voltage")} value={fmtVoltage(v?.battery?.voltage ?? null)} />
          <SensorField
            label={t("telemetry.sensors.current")}
            value={v?.battery?.current != null ? `${fmtNum(v.battery.current, 2)} A` : "—"}
          />
          <SensorField label={t("telemetry.sensors.remaining")} value={fmtPct(v?.battery?.remaining ?? null)} />
        </CardContent>
      </Card>
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">{t("telemetry.sensors.gps")}</div>
          <SensorField label={t("telemetry.sensors.fixType")} value={v?.gps?.fix_type != null ? String(v.gps.fix_type) : "—"} />
          <SensorField label={t("telemetry.sensors.sats")} value={v?.gps?.satellites != null ? String(v.gps.satellites) : "—"} />
          <SensorField label={t("telemetry.sensors.hdop")} value={fmtNum(v?.gps?.eph ?? null, 2)} />
          <SensorField
            label={t("telemetry.sensors.position")}
            value={hasPosition ? `${fmtNum(v!.position!.lat!, 5)}, ${fmtNum(v!.position!.lon!, 5)}` : "—"}
          />
          <SensorField
            label={t("telemetry.sensors.altRel")}
            value={v?.position?.alt_rel != null ? `${fmtNum(v.position.alt_rel, 1)} m` : "—"}
          />
        </CardContent>
      </Card>
      <Card className="md:col-span-2 lg:col-span-3">
        <CardContent className="pt-4 pb-4 grid grid-cols-2 gap-x-6">
          <SensorField label={t("telemetry.sensors.mode")} value={v?.mode ?? "—"} />
          <SensorField
            label={t("telemetry.sensors.armed")}
            value={v?.armed === true ? t("telemetry.sensors.armedValue") : v?.armed === false ? t("telemetry.sensors.disarmedValue") : "—"}
          />
        </CardContent>
      </Card>
      <p className="md:col-span-2 lg:col-span-3 text-[11px] text-muted-foreground">
        <Trans
          i18nKey="telemetry.sensors.editHint"
          components={{ link: <Link to="/telemetry" className="text-primary hover:underline" /> }}
        />
      </p>
    </div>
  );
}
