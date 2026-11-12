import { Pause, Play, ScrollText } from "lucide-react";
import { Link } from "react-router-dom";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import { PageShell } from "@/components/page-shell";
import { ReadErrorCard } from "@/components/read-error-card";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useResource } from "@/hooks/use-resource";
import {
  EMPTY_TAIL,
  LEVEL_KEYS,
  LOG_LEVELS,
  appendToTail,
  fmtLogTime,
  foldLevel,
  logsPollPath,
  logsReadFailed,
  openLogStream,
  scheduleStreamRetry,
  type LiveTail,
  type LogLevel,
  type LogsResponse,
} from "@/lib/logs-stream";
import { logsEmptyState } from "@/shared/api/logs-empty";

const TONE: Record<string, string> = {
  DEBUG: "text-muted-foreground",
  INFO: "text-foreground",
  WARN: "text-warn",
  ERROR: "text-destructive",
};

export function LogsRoute() {
  const { t } = useTranslation();
  const [level, setLevel] = useState<LogLevel>("");
  const [service, setService] = useState("");
  const [paused, setPaused] = useState(false);

  // Polling fallback used when the stream answers 503 (subscriber cap hit) or
  // drops. Also polled while the stream is up so the page always knows
  // whether the logging store is answering at all (its `warning`).
  const [pollFallback, setPollFallback] = useState(false);
  const logs = useResource<LogsResponse>(
    `logs:${level}:${service}:${pollFallback ? "poll" : "sse"}`,
    logsPollPath(level, service),
    paused ? false : pollFallback ? 2000 : 30000,
  );

  // Live tail from the stream, bounded and de-duplicated by `seq` (the agent
  // replays a snapshot on every open). Reset when the filters change.
  const [tail, setTail] = useState<LiveTail>(EMPTY_TAIL);
  useEffect(() => {
    setTail(EMPTY_TAIL);
  }, [level, service]);

  // The stream is read with fetch, which carries the credential headers a
  // paired node needs off-box (an EventSource cannot).
  useEffect(() => {
    if (paused || pollFallback) return undefined;
    const ctrl = new AbortController();
    void openLogStream(level, service, ctrl.signal, {
      onOpen: () => setPollFallback(false),
      onPayloads: (payloads) => setTail((prev) => appendToTail(prev, payloads)),
      onClosed: () => setPollFallback(true),
    });
    return () => ctrl.abort();
  }, [level, service, paused, pollFallback]);

  // On the polling fallback, try the stream again on a timer.
  useEffect(() => {
    if (paused || !pollFallback) return undefined;
    return scheduleStreamRetry(() => setPollFallback(false));
  }, [paused, pollFallback]);

  // Tab returns to visible on the polling fallback: try the stream again.
  useEffect(() => {
    function onVisibility() {
      if (!document.hidden) {
        setPollFallback((current) => (current ? false : current));
      }
    }
    document.addEventListener("visibilitychange", onVisibility);
    return () =>
      document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  // Source of truth for the rendered list: the live tail while the stream is
  // up, the polled entries on the fallback.
  const fallbackEntries = logs.data?.entries ?? [];
  const entries = pollFallback ? fallbackEntries : tail.entries;
  const empty = logsEmptyState({
    loading: logs.isLoading && pollFallback,
    filtered: Boolean(service || level),
    warning: logs.data?.warning,
  });

  return (
    <PageShell
      title={t("logs.page.title")}
      blurb={t("logs.page.blurb")}
      maxWidth="max-w-6xl"
      rightAction={
        <Button
          variant="outline"
          size="sm"
          onClick={() => setPaused((p) => !p)}
        >
          {paused ? (
            <>
              <Play className="h-3.5 w-3.5" /> {t("logs.page.resume")}
            </>
          ) : (
            <>
              <Pause className="h-3.5 w-3.5" /> {t("logs.page.pause")}
            </>
          )}
        </Button>
      }
    >
      <Card>
        <CardContent className="pt-4 pb-4 grid grid-cols-1 sm:grid-cols-[120px_1fr_auto] gap-3 items-end">
          <div className="space-y-1.5">
            <Label className="text-xs">{t("logs.filter.level")}</Label>
            <select
              value={level}
              onChange={(e) => setLevel(e.target.value as LogLevel)}
              className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
            >
              {LOG_LEVELS.map((l) => (
                <option key={l} value={l}>
                  {l ? t(LEVEL_KEYS[l]) : t("logs.filter.anyLevel")}
                </option>
              ))}
            </select>
          </div>

          <div className="space-y-1.5">
            <Label className="text-xs">{t("logs.filter.logger")}</Label>
            <Input
              placeholder={t("logs.filter.loggerPlaceholder")}
              value={service}
              onChange={(e) => setService(e.target.value)}
            />
          </div>

          <div className="text-xs text-muted-foreground font-mono pb-2 space-y-0.5 text-right">
            <div>
              {entries.length}
              {pollFallback ? t("logs.status.poll") : t("logs.status.live")}
            </div>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="p-0">
          {entries.length === 0 && logsReadFailed({ pollFallback, pollError: logs.isError }) ? (
            <div className="p-4">
              <ReadErrorCard message={t("logs.page.readFailed")} onRetry={() => void logs.refetch()} />
            </div>
          ) : entries.length === 0 ? (
            <div className="px-4 py-8 flex items-start gap-3 text-sm text-muted-foreground">
              <ScrollText className="h-4 w-4 mt-0.5" />
              <div className="space-y-1">
                <div>
                  {empty.kind === "loading"
                    ? t("logs.empty.loading")
                    : empty.kind === "storeOff"
                      ? t("logs.empty.storeOff")
                      : empty.kind === "storeError"
                        ? t("logs.empty.storeError", { warning: empty.warning })
                        : empty.kind === "noMatch"
                          ? t("logs.empty.noMatch")
                          : t("logs.empty.noEntries")}
                </div>
                {empty.kind === "storeOff" ? (
                  <div className="text-xs opacity-70">
                    <Link to="/settings/advanced#log-store" className="underline">
                      {t("settingsMore.advanced.logStoreLabel")}
                    </Link>
                  </div>
                ) : empty.kind === "noMatch" || empty.kind === "noEntries" ? (
                  <div className="text-xs opacity-70">{t("logs.empty.hint")}</div>
                ) : null}
              </div>
            </div>
          ) : (
            <ul className="font-mono text-xs divide-y divide-border/50 max-h-[70vh] overflow-y-auto">
              {entries.map((e, i) => {
                const lvl = foldLevel(e.level);
                return (
                  <li
                    key={`${e.seq ?? i}-${e.timestamp}`}
                    className="px-3 py-1.5 flex flex-wrap gap-x-3 gap-y-0.5"
                  >
                    <span className="text-muted-foreground shrink-0 tabular-nums">
                      {fmtLogTime(e.timestamp)}
                    </span>
                    <span className={`shrink-0 w-14 ${TONE[lvl] ?? ""}`}>
                      {lvl}
                    </span>
                    <span className="shrink-0 text-muted-foreground truncate max-w-[220px]">
                      {e.logger}
                    </span>
                    <span className="min-w-0 flex-[1_1_16rem] whitespace-pre-wrap break-words">
                      {e.message}
                    </span>
                  </li>
                );
              })}
            </ul>
          )}
        </CardContent>
      </Card>
    </PageShell>
  );
}
