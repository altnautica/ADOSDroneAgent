// Logs: a live tail of the agent's structured logger. The stream (`GET
// /api/logs/stream`, SSE) feeds a bounded, seq-de-duplicated tail; when it
// cannot open or drops (a 503 at the subscriber cap among them) the screen
// polls `GET /api/logs` every 2 s instead and retries the stream when the page
// becomes visible again. The poll also runs slowly beside a live stream so the
// screen knows whether the logging store answers at all (its `warning`). The
// list is virtualised by hand: fixed-height rows, only the visible window in
// the DOM.

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { useNavStore } from "@/stores/nav-store";
import { ActionButton, EmptyNote, StaleBadge } from "@/components/ui/data";
import { SelectSettingRow, TextSettingRow } from "@/components/ui/setting-row";
import {
  appendToTail,
  EMPTY_TAIL,
  fmtLogTime,
  LOG_LEVELS,
  logsPollPath,
  openLogStream,
  parseLogsResponse,
  visibleWindow,
  type LiveTail,
  type LogEntry,
  type LogLevel,
} from "@/lib/logs-stream";
import { cn } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import { logsEmptyState } from "@/shared/api/logs-empty";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const POLL_FALLBACK_MS = 2000;
const POLL_BESIDE_STREAM_MS = 30_000;
const ROW_PX = 26;

const LEVEL_CLASS: Record<string, string> = {
  DEBUG: "text-muted-foreground",
  INFO: "text-surface-foreground",
  WARNING: "text-warn",
  ERROR: "text-err",
  CRITICAL: "text-err font-semibold",
};

export function LogsScreen() {
  const { t } = useTranslation();
  const [level, setLevel] = useState<LogLevel>("");
  const [service, setService] = useState("");
  const [paused, setPaused] = useState(false);
  const [pollFallback, setPollFallback] = useState(false);
  const [tail, setTail] = useState<LiveTail>(EMPTY_TAIL);

  const poll = useResource(
    useCallback(
      (s: AbortSignal) => apiFetch<unknown>(logsPollPath(level, service), { signal: s }).then(parseLogsResponse),
      [level, service],
    ),
    pollFallback ? POLL_FALLBACK_MS : POLL_BESIDE_STREAM_MS,
    { enabled: !paused },
  );

  // A filter change restarts the tail from the stream's own snapshot.
  const changeLevel = (next: LogLevel) => {
    setTail(EMPTY_TAIL);
    setLevel(next);
  };
  const changeService = (next: string) => {
    setTail(EMPTY_TAIL);
    setService(next);
  };

  useEffect(() => {
    if (paused || pollFallback) return undefined;
    const ctrl = new AbortController();
    void openLogStream(level, service, ctrl.signal, {
      onOpen: () => undefined,
      onPayloads: (payloads) => setTail((prev) => appendToTail(prev, payloads)),
      onClosed: () => setPollFallback(true),
    });
    return () => ctrl.abort();
  }, [level, service, paused, pollFallback]);

  // Back on screen while polling: try the stream again.
  useEffect(() => {
    const onVisibility = () => {
      if (!document.hidden) setPollFallback(false);
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  const entries = pollFallback ? (poll.data?.entries ?? []) : tail.entries;
  const empty = logsEmptyState({
    loading: pollFallback && !poll.ready,
    filtered: Boolean(level || service.trim()),
    warning: poll.data?.warning,
  });

  let emptyText = t("logs.empty.noEntries");
  if (empty.kind === "loading") emptyText = t("logs.empty.loading");
  else if (empty.kind === "storeOff") emptyText = t("logs.empty.storeOff");
  else if (empty.kind === "storeError") emptyText = t("logs.empty.storeError", { warning: empty.warning });
  else if (empty.kind === "noMatch") emptyText = t("logs.empty.noMatch");

  return (
    <ConsoleScaffold
      title={t("logs.page.title")}
      onBack={() => useNavStore.getState().back()}
      actions={
        <div className="flex items-center gap-[0.4rem]">
          <span className="font-mono text-[0.75rem] text-muted-foreground">
            {t(pollFallback ? "cockpit.logs.countPoll" : "cockpit.logs.countLive", { count: entries.length })}
          </span>
          <StaleBadge stale={pollFallback && poll.stale} />
          <ActionButton
            label={paused ? t("logs.page.resume") : t("logs.page.pause")}
            onClick={() => setPaused((p) => !p)}
          />
        </div>
      }
    >
      <div className="flex h-full min-h-0 flex-col gap-[0.4rem]">
        <div className="grid grid-cols-1 gap-[0.3rem] sm:grid-cols-2">
          <SelectSettingRow<LogLevel>
            title={t("logs.filter.level")}
            options={LOG_LEVELS}
            value={level}
            optionLabel={(l) => l || t("logs.filter.anyLevel")}
            onSelect={changeLevel}
          />
          <TextSettingRow
            title={t("logs.filter.logger")}
            value={service}
            onChange={changeService}
            placeholder={t("logs.filter.loggerPlaceholder")}
          />
        </div>
        {entries.length === 0 ? (
          <EmptyNote>
            {emptyText}
            {empty.kind === "storeOff" ? (
              <span className="mt-[0.3rem] block text-[0.75rem] opacity-80">{t("logs.empty.storeOffHint")}</span>
            ) : empty.kind === "noMatch" || empty.kind === "noEntries" ? (
              <span className="mt-[0.3rem] block text-[0.75rem] opacity-80">{t("logs.empty.hint")}</span>
            ) : null}
          </EmptyNote>
        ) : (
          <VirtualLog entries={entries} follow={!paused} />
        )}
      </div>
    </ConsoleScaffold>
  );
}

function VirtualLog({ entries, follow }: { entries: LogEntry[]; follow: boolean }) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDivElement | null>(null);
  const [scroll, setScroll] = useState({ top: 0, height: 0 });
  // Stick to the newest row unless the operator scrolled up.
  const pinned = useRef(true);

  useEffect(() => {
    const el = ref.current;
    if (!el) return undefined;
    const measure = () => setScroll({ top: el.scrollTop, height: el.clientHeight });
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (el && follow && pinned.current) el.scrollTop = el.scrollHeight;
  }, [entries, follow]);

  const { start, end } = visibleWindow(entries.length, ROW_PX, scroll.top, scroll.height);

  return (
    <div
      ref={ref}
      role="log"
      aria-label={t("logs.page.title")}
      className="min-h-0 flex-1 overflow-y-auto rounded-md border border-border font-mono text-[0.75rem]"
      onScroll={(e) => {
        const el = e.currentTarget;
        pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < ROW_PX * 2;
        setScroll({ top: el.scrollTop, height: el.clientHeight });
      }}
    >
      <div style={{ height: entries.length * ROW_PX, position: "relative" }}>
        {entries.slice(start, end).map((e, i) => {
          const index = start + i;
          return (
            <div
              key={`${e.seq ?? index}-${String(e.timestamp)}`}
              className="absolute inset-x-0 flex items-center gap-[0.6rem] px-[0.6rem]"
              style={{ top: index * ROW_PX, height: ROW_PX }}
            >
              <span className="shrink-0 tabular-nums text-muted-foreground">{fmtLogTime(e.timestamp)}</span>
              <span className={cn("w-[4.5rem] shrink-0", LEVEL_CLASS[e.level] ?? "")}>{e.level}</span>
              <span className="max-w-[14rem] shrink-0 truncate text-muted-foreground">{e.logger}</span>
              <span className="min-w-0 flex-1 truncate" title={e.message}>
                {e.message}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
