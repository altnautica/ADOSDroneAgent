// The Logs screen's data plumbing, kept free of React so it is unit-testable:
// the filter query, the Server-Sent-Events frame parser for `GET
// /api/logs/stream`, the bounded de-duplicating live tail, and the window math
// of the hand-rolled virtual list.
//
// The stream is read with `fetch` rather than `EventSource` because a paired
// node reached off-box needs the credential headers on every request, and an
// EventSource cannot carry a header.

import { credentialHeaders } from "@/shared/api-fetch";

export interface LogEntry {
  seq?: number;
  /** Unix seconds, an ISO string, or null when the store row carried none. */
  timestamp: number | string | null;
  level: string;
  logger: string;
  message: string;
}

export interface LogsResponse {
  entries: LogEntry[];
  total?: number;
  /** Set when the logging store could not answer (off by default). */
  warning?: string;
}

export const LOG_LEVELS = ["", "DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"] as const;
export type LogLevel = (typeof LOG_LEVELS)[number];

/** Entries the live tail keeps, to bound memory on a screen left open. */
export const LIVE_TAIL_CAP = 2500;

/** The `level` / `service` query shared by the stream and the poll. */
export function logsQuery(level: LogLevel, service: string): string {
  const params = new URLSearchParams();
  if (level) params.set("level", level);
  const svc = service.trim();
  if (svc) params.set("service", svc);
  return params.toString();
}

/** The poll fallback path. */
export function logsPollPath(level: LogLevel, service: string): string {
  const q = logsQuery(level, service);
  return `/api/logs?limit=200${q ? `&${q}` : ""}`;
}

function isEntry(v: unknown): v is LogEntry {
  if (typeof v !== "object" || v === null) return false;
  const e = v as Record<string, unknown>;
  return typeof e.message === "string" && typeof e.level === "string";
}

/**
 * Split buffered SSE text into complete events. Returns the `data:` payload
 * of each complete event (multi-line data joined with `\n`; comment-only and
 * keep-alive frames dropped) and the unterminated remainder to keep buffering.
 */
export function parseSseChunk(buffer: string): { events: string[]; rest: string } {
  const normalized = buffer.replace(/\r\n?/g, "\n");
  const blocks = normalized.split("\n\n");
  const rest = blocks.pop() ?? "";
  const events: string[] = [];
  for (const block of blocks) {
    const data = block
      .split("\n")
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(line.startsWith("data: ") ? 6 : 5));
    if (data.length > 0) events.push(data.join("\n"));
  }
  return { events, rest };
}

export interface LiveTail {
  entries: LogEntry[];
  /** Highest `seq` seen; the agent replays a snapshot on every (re)connect. */
  lastSeq: number;
}

export const EMPTY_TAIL: LiveTail = { entries: [], lastSeq: 0 };

/** Fold stream payloads into the tail: malformed frames are skipped, a `seq`
 *  at or below the last one seen is a replay and dropped, and the tail keeps
 *  the newest `cap` entries. */
export function appendToTail(tail: LiveTail, payloads: string[], cap = LIVE_TAIL_CAP): LiveTail {
  let lastSeq = tail.lastSeq;
  const added: LogEntry[] = [];
  for (const raw of payloads) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      continue;
    }
    if (!isEntry(parsed)) continue;
    if (typeof parsed.seq === "number") {
      if (parsed.seq <= lastSeq) continue;
      lastSeq = parsed.seq;
    }
    added.push(parsed);
  }
  if (added.length === 0) return tail;
  const next = tail.entries.concat(added);
  return { entries: next.length > cap ? next.slice(next.length - cap) : next, lastSeq };
}

/** Parse the poll body defensively. */
export function parseLogsResponse(body: unknown): LogsResponse {
  const b = (typeof body === "object" && body !== null ? body : {}) as Record<string, unknown>;
  return {
    entries: Array.isArray(b.entries) ? b.entries.filter(isEntry) : [],
    total: typeof b.total === "number" ? b.total : undefined,
    warning: typeof b.warning === "string" ? b.warning : undefined,
  };
}

/** Wall-clock text for an entry's time. */
export function fmtLogTime(ts: LogEntry["timestamp"]): string {
  if (ts === null) return "—";
  if (typeof ts === "number") return new Date(ts * 1000).toLocaleTimeString();
  return ts;
}

/** The rows a fixed-row-height list renders for a scroll position, with
 *  `overscan` rows either side. `end` is exclusive. */
export function visibleWindow(
  count: number,
  rowHeight: number,
  scrollTop: number,
  viewportHeight: number,
  overscan = 8,
): { start: number; end: number } {
  if (count <= 0 || rowHeight <= 0) return { start: 0, end: 0 };
  const first = Math.floor(Math.max(0, scrollTop) / rowHeight);
  const shown = Math.ceil(Math.max(0, viewportHeight) / rowHeight) + 1;
  const start = Math.max(0, Math.min(count - 1, first) - overscan);
  const end = Math.min(count, first + shown + overscan);
  return { start, end };
}

export interface LogStreamHandlers {
  onOpen: () => void;
  onPayloads: (payloads: string[]) => void;
  /** The stream ended or could not open (a 503 at the subscriber cap among
   *  them); the caller falls back to polling. */
  onClosed: () => void;
}

/** Open `GET /api/logs/stream` with the credential headers and feed complete
 *  events to `onPayloads`. Abort the signal to close it. */
export async function openLogStream(
  level: LogLevel,
  service: string,
  signal: AbortSignal,
  handlers: LogStreamHandlers,
): Promise<void> {
  const q = logsQuery(level, service);
  try {
    const res = await fetch(`/api/logs/stream${q ? `?${q}` : ""}`, {
      headers: { Accept: "text/event-stream", ...credentialHeaders() },
      signal,
    });
    if (!res.ok || !res.body) {
      if (!signal.aborted) handlers.onClosed();
      return;
    }
    handlers.onOpen();
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const { events, rest } = parseSseChunk(buffer);
      buffer = rest;
      if (events.length > 0) handlers.onPayloads(events);
    }
  } catch {
    // An abort or a dropped connection; handled below.
  }
  if (!signal.aborted) handlers.onClosed();
}
