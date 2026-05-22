import { Pause, Play, ScrollText } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { PageShell } from "@/components/page-shell";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useResource } from "@/hooks/use-resource";

interface LogEntry {
  seq?: number;
  timestamp: number | string;
  level: string;
  logger: string;
  message: string;
}

interface LogsResponse {
  entries: LogEntry[];
  total: number;
  limit: number;
  offset: number;
  buffer_size?: number;
  buffer_cap?: number;
}

const LEVELS = ["", "DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"] as const;
type Level = (typeof LEVELS)[number];

// Max entries the live tail keeps in memory on the dashboard. The agent
// buffers 5000; we keep half to bound DOM cost as the page stays open.
const LIVE_TAIL_CAP = 2500;

const TONE: Record<string, string> = {
  DEBUG: "text-muted-foreground/70",
  INFO: "text-foreground",
  WARNING: "text-warn",
  ERROR: "text-destructive",
  CRITICAL: "text-destructive font-semibold",
};

function fmtTs(ts: LogEntry["timestamp"]): string {
  if (typeof ts === "number") {
    return new Date(ts * 1000).toLocaleTimeString();
  }
  return String(ts);
}

export function LogsRoute() {
  const [level, setLevel] = useState<Level>("");
  const [service, setService] = useState("");
  const [paused, setPaused] = useState(false);

  // Polling fallback used when SSE returns 503 (subscriber cap hit) or
  // any other error. Also drives the buffer_size / buffer_cap hint
  // even when SSE is active so the dashboard always knows the buffer
  // wrap state.
  const path = `/api/logs?limit=200${level ? `&level=${level}` : ""}${
    service ? `&service=${encodeURIComponent(service)}` : ""
  }`;
  const [pollFallback, setPollFallback] = useState(false);
  const logs = useResource<LogsResponse>(
    `logs:${level}:${service}:${pollFallback ? "poll" : "sse"}`,
    path,
    paused ? false : pollFallback ? 2000 : 30000,
  );

  // Live tail accumulated from the SSE connection. Bounded to
  // LIVE_TAIL_CAP entries so the DOM doesn't grow without bound on a
  // long-open page.
  const [liveEntries, setLiveEntries] = useState<LogEntry[]>([]);
  const seenSeq = useRef<number>(0);

  useEffect(() => {
    if (paused) return undefined;

    let cancelled = false;
    const params = new URLSearchParams();
    if (level) params.set("level", level);
    if (service) params.set("service", service);
    const url = `/api/logs/stream${params.toString() ? `?${params}` : ""}`;
    const source = new EventSource(url, { withCredentials: false });

    source.onopen = () => {
      if (!cancelled) setPollFallback(false);
    };

    source.onmessage = (ev: MessageEvent<string>) => {
      if (cancelled) return;
      try {
        const entry = JSON.parse(ev.data) as LogEntry;
        // Sequence-number de-dup across reconnects: the agent's SSE
        // sends a 100-entry snapshot on every open, and EventSource
        // auto-reconnects on disconnect. Track the last-seen seq so
        // we don't render the same entry twice.
        if (typeof entry.seq === "number") {
          if (entry.seq <= seenSeq.current) return;
          seenSeq.current = entry.seq;
        }
        setLiveEntries((prev) => {
          const next = [...prev, entry];
          return next.length > LIVE_TAIL_CAP
            ? next.slice(next.length - LIVE_TAIL_CAP)
            : next;
        });
      } catch {
        // Malformed frame; ignore.
      }
    };

    source.onerror = () => {
      if (cancelled) return;
      // EventSource will auto-reconnect on transient errors. We flip
      // to polling only after EventSource gives up (readyState
      // CLOSED) so the page still shows entries via /api/logs.
      if (source.readyState === EventSource.CLOSED) {
        setPollFallback(true);
      }
    };

    return () => {
      cancelled = true;
      source.close();
    };
  }, [level, service, paused]);

  // Reset live tail when filters change so the operator sees a clean
  // restart of the stream rather than a mix of pre- and post-filter
  // entries.
  useEffect(() => {
    setLiveEntries([]);
    seenSeq.current = 0;
  }, [level, service]);

  // Tab-hidden = close SSE indirectly by pausing; resume on visible.
  // EventSource itself stays open while hidden, but the dashboard's
  // useResource polling is throttled by react-query already; this
  // hook keeps the SSE reconnect counter from churning.
  useEffect(() => {
    function onVisibility() {
      // Only auto-resume; never auto-pause (operator pauses
      // explicitly). The browser already suspends EventSource
      // activity when hidden.
      if (!document.hidden) {
        setPollFallback((f) => f);
      }
    }
    document.addEventListener("visibilitychange", onVisibility);
    return () =>
      document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  // Source of truth for the rendered list:
  // - SSE healthy → liveEntries (real-time tail).
  // - SSE failed and fell back to polling → /api/logs entries.
  const fallbackEntries = logs.data?.entries ?? [];
  const entries = pollFallback ? fallbackEntries : liveEntries;

  return (
    <PageShell
      title="Logs"
      blurb="Live tail of the agent's structured logger. Filter by level or logger name."
      maxWidth="max-w-6xl"
      rightAction={
        <Button
          variant="outline"
          size="sm"
          onClick={() => setPaused((p) => !p)}
        >
          {paused ? (
            <>
              <Play className="h-3.5 w-3.5" /> Resume
            </>
          ) : (
            <>
              <Pause className="h-3.5 w-3.5" /> Pause
            </>
          )}
        </Button>
      }
    >
      <Card>
        <CardContent className="pt-4 pb-4 grid grid-cols-1 sm:grid-cols-[120px_1fr_auto] gap-3 items-end">
          <div className="space-y-1.5">
            <Label className="text-xs">Level</Label>
            <select
              value={level}
              onChange={(e) => setLevel(e.target.value as Level)}
              className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm font-mono"
            >
              {LEVELS.map((l) => (
                <option key={l} value={l}>
                  {l || "any"}
                </option>
              ))}
            </select>
          </div>

          <div className="space-y-1.5">
            <Label className="text-xs">Logger contains</Label>
            <Input
              placeholder="filter by logger name (optional)"
              value={service}
              onChange={(e) => setService(e.target.value)}
            />
          </div>

          <div className="text-xs text-muted-foreground font-mono pb-2 space-y-0.5 text-right">
            <div>
              {entries.length}
              {pollFallback ? " (poll)" : " (live)"}
            </div>
            {logs.data?.buffer_size != null && (
              <div className="text-[10px] opacity-70">
                buffer {logs.data.buffer_size}/{logs.data.buffer_cap ?? "?"}
              </div>
            )}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="p-0">
          {entries.length === 0 ? (
            <div className="px-4 py-8 flex items-start gap-3 text-sm text-muted-foreground">
              <ScrollText className="h-4 w-4 mt-0.5" />
              <div className="space-y-1">
                <div>
                  {logs.isLoading && pollFallback
                    ? "loading…"
                    : service || level
                      ? "no log entries match the filter."
                      : "no log entries buffered yet."}
                </div>
                {!logs.isLoading && (
                  <div className="text-xs opacity-70">
                    The agent buffers up to {logs.data?.buffer_cap ?? 5000}{" "}
                    entries since process start. Drop filters or restart a
                    service to see fresh activity.
                  </div>
                )}
              </div>
            </div>
          ) : (
            <ul className="font-mono text-xs divide-y divide-border/50 max-h-[70vh] overflow-y-auto">
              {entries.map((e, i) => (
                <li
                  key={`${e.seq ?? i}-${e.timestamp}`}
                  className="px-3 py-1.5 flex gap-3"
                >
                  <span className="text-muted-foreground/60 shrink-0 tabular-nums">
                    {fmtTs(e.timestamp)}
                  </span>
                  <span
                    className={`shrink-0 w-14 ${TONE[e.level] ?? ""}`}
                  >
                    {e.level}
                  </span>
                  <span className="shrink-0 text-muted-foreground/70 truncate max-w-[220px]">
                    {e.logger}
                  </span>
                  <span className="flex-1 truncate" title={e.message}>
                    {e.message}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>
    </PageShell>
  );
}
