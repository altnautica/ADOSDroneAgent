import { useVirtualizer } from "@tanstack/react-virtual";
import { Radio, RotateCcw, Save, Search } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";

import { PageShell } from "@/components/page-shell";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useResource } from "@/hooks/use-resource";
import { useSnapshot } from "@/hooks/use-snapshot";
import { ApiError, apiFetch } from "@/lib/api";
import {
  buildRows,
  categoryCounts,
  filterRows,
  formatParamValue,
  type FilterState,
  type ParamRow,
} from "@/lib/params";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { useParamsStore } from "@/stores/params-store";

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
  const [tab, setTab] = useState<"parameters" | "sensors">("parameters");

  return (
    <PageShell
      title="Telemetry"
      blurb="Flight controller parameters and live sensors. Edits are validated against the cache before they hit the FC."
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
            Parameters
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
            Sensors
          </button>
        </div>
      }
    >
      {tab === "parameters" ? <ParametersTab /> : <SensorsTab />}
    </PageShell>
  );
}

function ParametersTab() {
  const params = useResource<ParamsResponse>("params", "/api/params", 10000);
  const { drafts, setDraft, clearAll } = useParamsStore();

  const [search, setSearch] = useState("");
  const [debouncedSearch, setDebouncedSearch] = useState("");
  const [category, setCategory] = useState<string | null>(null);
  const [modifiedOnly, setModifiedOnly] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [saveProgress, setSaveProgress] = useState<{
    total: number;
    done: number;
  } | null>(null);

  // Debounce the search input so typing doesn't re-filter the whole
  // list on every keystroke (param tables can run 800+ rows).
  useEffect(() => {
    const t = setTimeout(() => setDebouncedSearch(search), 150);
    return () => clearTimeout(t);
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
        toast.err(`${name}: failed`, msg);
      }
      setSaveProgress({ total: items.length, done: i + 1 });
    }

    setSaveProgress(null);
    params.refetch();
    if (failCount === 0 && pendingCount === 0) {
      toast.ok(`Saved ${okCount} parameter${okCount === 1 ? "" : "s"}.`);
    } else {
      toast.info(
        `Saved ${okCount}, ${failCount} failed, ${pendingCount} pending FC ack.`,
        "FC may take longer than 2s to echo PARAM_VALUE under heavy load.",
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
          <span className="font-mono">All</span>
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
              placeholder="Search by name (e.g. BATT_LOW_VOLT)…"
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
            Modified only
          </label>
          <Button
            size="sm"
            variant="outline"
            disabled={dirtyCount === 0 || !!saveProgress}
            onClick={() => clearAll()}
          >
            <RotateCcw className="h-3.5 w-3.5" />
            Discard
          </Button>
          <Button
            size="sm"
            disabled={dirtyCount === 0 || !!saveProgress}
            onClick={() => setConfirmOpen(true)}
          >
            <Save className="h-3.5 w-3.5" />
            {saveProgress
              ? `${saveProgress.done}/${saveProgress.total}…`
              : `Save ${dirtyCount || ""}`}
          </Button>
        </div>

        {params.isLoading && (
          <p className="text-sm text-muted-foreground">loading parameters…</p>
        )}

        {params.isError && (
          <Card>
            <CardContent className="pt-5 pb-5 flex items-center justify-between gap-3">
              <p className="text-sm text-destructive">
                Couldn't reach the parameter service.
              </p>
              <Button
                size="sm"
                variant="outline"
                onClick={() => params.refetch?.()}
              >
                Retry
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
                  Priming parameter cache from FC…
                </div>
                <div className="text-xs text-muted-foreground">
                  {params.data.progress?.got ?? 0} of{" "}
                  {params.data.progress?.expected ?? "?"} parameters received.
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
                    Couldn't reach the flight controller.
                  </div>
                  <div className="text-xs text-muted-foreground">
                    {params.data?.priming_send_failed
                      ? "The agent could not send PARAM_REQUEST_LIST over the MAVLink link. Check the FC serial cable, baud rate, and that the FC is powered."
                      : "The agent sent PARAM_REQUEST_LIST but the flight controller did not respond within 30 seconds. Check the cable, baud rate, and that the FC firmware is alive."}
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
                    No parameters cached yet.
                  </div>
                  <div className="text-xs text-muted-foreground">
                    Connect a flight controller and the agent fires
                    PARAM_REQUEST_LIST on connect so the table fills in
                    automatically.
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
                      <span className="flex-1 truncate" title={row.name}>
                        {row.name}
                      </span>
                      <span className="text-muted-foreground w-24 text-right tabular-nums">
                        {formatParamValue(row.value)}
                      </span>
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
                    </div>
                  );
                })}
              </div>
            </div>
          </Card>
        )}

        {visibleRows.length === 0 && rows.length > 0 && (
          <p className="text-sm text-muted-foreground">
            No parameters match the current filter.
          </p>
        )}
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={`Save ${dirtyCount} parameter${dirtyCount === 1 ? "" : "s"}?`}
        description={
          <div className="space-y-2">
            <p>
              The agent will write each parameter to the FC and wait for a
              PARAM_VALUE ack (up to 2s per param). ArduPilot saves to
              EEPROM on receipt — there's no separate flash step.
            </p>
            <p className="text-xs text-muted-foreground">
              Bad parameter values can affect flight behaviour. Test on a
              bench rig first.
            </p>
          </div>
        }
        confirmLabel="Save all"
        destructive
        onConfirm={saveAll}
      />
    </div>
  );
}

function SensorsTab() {
  const snap = useSnapshot();
  const sensors = snap.data?.sensors ?? [];

  if (sensors.length === 0) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 flex items-start gap-3">
          <Radio className="h-5 w-5 text-muted-foreground mt-0.5" />
          <div>
            <div className="text-sm font-medium">No sensors reported.</div>
            <div className="text-xs text-muted-foreground mt-1">
              Sensors come from the FC's vehicle-state stream. Connect a
              FC, plug in peripherals (rangefinder, optical flow, airspeed,
              barometer), and the list populates.
            </div>
          </div>
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
      {sensors.map((s) => (
        <Card key={s.id}>
          <CardContent className="pt-4 pb-4 space-y-1.5">
            <div className="flex items-center gap-2">
              <span className="font-mono text-sm">{s.id}</span>
              {s.state && (
                <span
                  className={cn(
                    "ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border",
                    s.state === "ok"
                      ? "border-ok/40 text-ok"
                      : s.state === "error" || s.state === "failed"
                        ? "border-destructive/40 text-destructive"
                        : "border-muted-foreground/40 text-muted-foreground",
                  )}
                >
                  {s.state}
                </span>
              )}
            </div>
            {s.name && (
              <div className="text-xs text-muted-foreground">{s.name}</div>
            )}
            {s.value !== undefined && s.value !== null && (
              <pre className="text-[11px] text-muted-foreground font-mono whitespace-pre-wrap break-all">
                {typeof s.value === "object"
                  ? JSON.stringify(s.value, null, 2)
                  : String(s.value)}
              </pre>
            )}
          </CardContent>
        </Card>
      ))}
      <p className="md:col-span-2 text-[11px] text-muted-foreground">
        Want to edit FC parameters? Use the{" "}
        <Link to="/telemetry" className="text-primary hover:underline">
          Parameters tab
        </Link>
        .
      </p>
    </div>
  );
}
