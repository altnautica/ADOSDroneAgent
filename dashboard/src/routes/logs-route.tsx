import { Pause, Play, ScrollText } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useResource } from "@/hooks/use-resource";

interface LogEntry {
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
}

const LEVELS = ["", "DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"] as const;
type Level = (typeof LEVELS)[number];

const TONE: Record<string, string> = {
  DEBUG: "text-muted-foreground/70",
  INFO: "text-foreground",
  WARNING: "text-amber-500",
  ERROR: "text-red-500",
  CRITICAL: "text-red-600 font-semibold",
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

  const path = `/api/logs?limit=200${level ? `&level=${level}` : ""}${
    service ? `&service=${encodeURIComponent(service)}` : ""
  }`;

  const logs = useResource<LogsResponse>(
    `logs:${level}:${service}`,
    path,
    paused ? false : 2000,
  );

  const entries = logs.data?.entries ?? [];

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
              placeholder="ados.services.video"
              value={service}
              onChange={(e) => setService(e.target.value)}
            />
          </div>

          <div className="text-xs text-muted-foreground font-mono pb-2">
            {logs.data?.total != null
              ? `${entries.length} of ${logs.data.total}`
              : "—"}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="p-0">
          {entries.length === 0 ? (
            <div className="px-4 py-8 flex items-center gap-2 text-sm text-muted-foreground">
              <ScrollText className="h-4 w-4" />
              {logs.isLoading
                ? "loading…"
                : "no log entries match. drop the filter or wait for activity."}
            </div>
          ) : (
            <ul className="font-mono text-xs divide-y divide-border/50 max-h-[70vh] overflow-y-auto">
              {entries.map((e, i) => (
                <li
                  key={`${i}-${e.timestamp}`}
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
