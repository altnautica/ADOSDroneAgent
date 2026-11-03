// The compute node's job API, read through the agent: the World Engine
// extension serves it on its plugin socket on a workstation or compute node,
// and the agent forwards `/api/plugins/<id>/x/<rest>` there. Every reply is
// coerced defensively; a field the node does not send reads null, never a
// made-up value.

import { apiFetch } from "@/shared/api-fetch";

export const WORLD_ENGINE_ID = "com.altnautica.world-engine";
const BASE = `/api/plugins/${WORLD_ENGINE_ID}/x/api/compute`;

export interface NodeStatus {
  role: string | null;
  queueDepth: number | null;
  activeJobs: number | null;
  workersIdle: number | null;
  activeSessions: number | null;
  clusterSlaves: number;
  sessionStates: { opening: number; live: number; stalled: number } | null;
}

export interface NodeJob {
  id: string;
  kind: string;
  state: string;
  /** 0..1 while running. */
  progress: number | null;
  error: string | null;
  sessionId: string | null;
  updatedMs: number | null;
}

export interface NodeOutput {
  id: string;
  kind: string;
  format: string | null;
  file: string | null;
  sizeBytes: number | null;
}

export interface OffloadSession {
  id: string;
  cameraId: string | null;
  state: string;
  framesProcessed: number | null;
  reconnects: number | null;
  restarts: number | null;
  uptimeMs: number | null;
}

type Obj = Record<string, unknown>;

function obj(v: unknown): Obj | null {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : null;
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function str(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}
function list(v: unknown): unknown[] {
  return Array.isArray(v) ? v : [];
}

export function parseNodeStatus(raw: unknown): NodeStatus | null {
  const o = obj(raw);
  if (!o) return null;
  const states = obj(o.session_states);
  return {
    role: str(o.role),
    queueDepth: num(o.queue_depth),
    activeJobs: num(o.active_jobs),
    workersIdle: num(o.workers_idle),
    activeSessions: num(o.active_sessions),
    clusterSlaves: list(obj(o.cluster)?.slaves).length,
    sessionStates: states
      ? { opening: num(states.opening) ?? 0, live: num(states.live) ?? 0, stalled: num(states.stalled) ?? 0 }
      : null,
  };
}

/** A job error arrives as a string or as `{message, code, ...}`. */
function jobError(v: unknown): string | null {
  if (typeof v === "string") return v || null;
  return str(obj(v)?.message);
}

export function parseNodeJobs(raw: unknown): NodeJob[] {
  return list(raw).flatMap((r) => {
    const o = obj(r);
    if (!o || typeof o.id !== "string") return [];
    return [
      {
        id: o.id,
        kind: str(o.kind) ?? "",
        state: str(o.state) ?? "",
        progress: num(o.progress),
        error: jobError(o.error),
        sessionId: str(o.session_id),
        updatedMs: num(o.updated_ms),
      },
    ];
  });
}

export function parseNodeOutputs(raw: unknown): NodeOutput[] {
  return list(raw).flatMap((r) => {
    const o = obj(r);
    if (!o || typeof o.id !== "string") return [];
    return [{ id: o.id, kind: str(o.kind) ?? "", format: str(o.format), file: str(o.file), sizeBytes: num(o.size_bytes) }];
  });
}

export function parseOffloadSessions(raw: unknown): OffloadSession[] {
  return list(raw).flatMap((r) => {
    const o = obj(r);
    if (!o || typeof o.id !== "string") return [];
    return [
      {
        id: o.id,
        cameraId: str(o.camera_id),
        state: str(o.state) ?? "",
        framesProcessed: num(o.frames_processed),
        reconnects: num(o.reconnects),
        restarts: num(o.restarts),
        uptimeMs: num(o.uptime_ms),
      },
    ];
  });
}

/** Whether a job state still runs (and so may be cancelled). */
export function isJobActive(state: string): boolean {
  return state === "queued" || state === "running";
}

export async function getNodeStatus(signal: AbortSignal): Promise<NodeStatus | null> {
  return parseNodeStatus(await apiFetch<unknown>(`${BASE}/status`, { signal }));
}

export async function getNodeJobs(signal: AbortSignal): Promise<NodeJob[]> {
  return parseNodeJobs(await apiFetch<unknown>(`${BASE}/jobs`, { signal }));
}

export async function getNodeOutputs(jobId: string, signal: AbortSignal): Promise<NodeOutput[]> {
  return parseNodeOutputs(await apiFetch<unknown>(`${BASE}/jobs/${encodeURIComponent(jobId)}/outputs`, { signal }));
}

export async function getOffloadSessions(signal: AbortSignal): Promise<OffloadSession[]> {
  return parseOffloadSessions(await apiFetch<unknown>(`${BASE}/sessions`, { signal }));
}

export async function cancelNodeJob(jobId: string): Promise<void> {
  await apiFetch<unknown>(`${BASE}/jobs/${encodeURIComponent(jobId)}/cancel`, { method: "POST" });
}
