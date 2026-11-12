// The Logs page's data plumbing, kept free of React so it is unit-testable:
// the filter query, the Server-Sent-Events frame parser for `GET
// /api/logs/stream`, the bounded de-duplicating live tail, and the row text.
// Same rules as the cockpit's Logs screen.
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

/** The filter levels: the agent emits DEBUG/INFO/WARN/ERROR (no CRITICAL). */
export const LOG_LEVELS = ["", "DEBUG", "INFO", "WARN", "ERROR"] as const;
export type LogLevel = (typeof LOG_LEVELS)[number];

/** The label key of each filter level. */
export const LEVEL_KEYS = {
  DEBUG: "logs.level.debug",
  INFO: "logs.level.info",
  WARN: "logs.level.warn",
  ERROR: "logs.level.error",
} as const;

/** Entries the live tail keeps, to bound memory on a page left open. */
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

/** Split buffered SSE text into complete events: the `data:` payload of each
 *  (multi-line data joined with `\n`; comment-only and keep-alive frames
 *  dropped) and the unterminated remainder to keep buffering. */
export function parseSseChunk(buffer: string): { events: string[]; rest: string } {
  const blocks = buffer.replace(/\r\n?/g, "\n").split("\n\n");
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

/** Wall-clock text for an entry's time. */
export function fmtLogTime(ts: LogEntry["timestamp"]): string {
  if (ts === null) return "—";
  const d = typeof ts === "number" ? new Date(ts * 1000) : new Date(ts);
  // An RFC 3339 string reads as local HH:MM:SS like a numeric stamp; text
  // that is not a time is shown as it came.
  return Number.isNaN(d.getTime()) ? String(ts) : d.toLocaleTimeString([], { hour12: false });
}

/** A level as the filter names it: WARNING folds to WARN and CRITICAL/FATAL
 *  to ERROR, so every producer's spelling lands in one bucket. */
export function foldLevel(level: string): string {
  const l = level.toUpperCase();
  if (l === "WARNING") return "WARN";
  if (l === "CRITICAL" || l === "FATAL") return "ERROR";
  return l;
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

/** How long the page stays on the polling fallback before it tries the
 *  stream again. */
export const STREAM_RETRY_MS = 15_000;

/** Call `retry` once after `STREAM_RETRY_MS`; the returned function cancels. */
export function scheduleStreamRetry(retry: () => void, ms: number = STREAM_RETRY_MS): () => void {
  const id = setTimeout(retry, ms);
  return () => clearTimeout(id);
}

/** The logs cannot be read: the stream is down (refused or dropped) and the
 *  polling fallback fails too. Not "no entries". */
export function logsReadFailed(i: { pollFallback: boolean; pollError: boolean }): boolean {
  return i.pollFallback && i.pollError;
}
