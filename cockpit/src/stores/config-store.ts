// The shared agent-config store for the Settings tree. One GET /api/config
// snapshot is cached here and read by every drill level + leaf editor, so the
// panel fetches the config once (not once per drill) and a drill-in shows the
// cached tree immediately instead of blanking while it re-fetches. A background
// poll keeps it honest against a change made elsewhere (GCS / CLI); every write
// refreshes it so a saved leaf reflects the persisted value on the next read.
//
// The reboot-pending set lives here too (persisted to localStorage so it
// survives a kiosk reload) and auto-clears once the box's uptime shows it has
// actually rebooted — the honest surface: the banner never lies about a
// reboot-gated change being live, and never lingers after the reboot happened.

import { useEffect } from "react";
import { create } from "zustand";

import { ApiError, apiFetch, getConfig } from "@/lib/api";
import type { AgentConfig } from "@/lib/types";

const REBOOT_PATHS_KEY = "ados-cockpit-reboot-paths";
const REBOOT_SINCE_KEY = "ados-cockpit-reboot-since";
const POLL_MS = 8000;
/** A booted-after margin so clock skew between the panel and the agent cannot
 *  clear the banner one poll early. */
const REBOOT_CLEAR_MARGIN_MS = 15_000;

/** The result of one `PUT /api/config` write, surfaced to the editor. */
export interface WriteResult {
  ok: boolean;
  /** True when the value was written to /etc/ados/config.yaml (not just memory). */
  persisted?: boolean;
  /** The coerced value the agent stored (its JSON echo of the write). */
  value?: unknown;
  /** A human error to show inline when `ok` is false (validation / not-found). */
  error?: string;
}

function loadPaths(): string[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const raw = localStorage.getItem(REBOOT_PATHS_KEY);
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.filter((p) => typeof p === "string") : [];
  } catch {
    return [];
  }
}

function loadSince(): number | null {
  if (typeof localStorage === "undefined") return null;
  try {
    const raw = localStorage.getItem(REBOOT_SINCE_KEY);
    const n = raw == null ? NaN : Number(raw);
    return Number.isFinite(n) ? n : null;
  } catch {
    return null;
  }
}

function persistReboot(paths: string[], since: number | null): void {
  if (typeof localStorage === "undefined") return;
  try {
    if (paths.length) {
      localStorage.setItem(REBOOT_PATHS_KEY, JSON.stringify(paths));
      localStorage.setItem(REBOOT_SINCE_KEY, since == null ? "" : String(since));
    } else {
      localStorage.removeItem(REBOOT_PATHS_KEY);
      localStorage.removeItem(REBOOT_SINCE_KEY);
    }
  } catch {
    // storage disabled — the in-memory state still drives the banner this session
  }
}

/** Shape of the FastAPI validation error body the config PUT raises (422/400). */
interface ConfigErrorBody {
  detail?: { error?: { message?: string; messages?: string[] } };
}

interface ConfigState {
  config: AgentConfig | null;
  ready: boolean;
  stale: boolean;
  error: string | null;
  /** Dot-paths written this session that need a reboot to take effect. */
  pendingRebootPaths: string[];
  /** Epoch ms the first pending path was recorded (for the uptime auto-clear). */
  pendingRebootSince: number | null;

  refresh: () => Promise<void>;
  write: (key: string, value: string) => Promise<WriteResult>;
  clearReboot: () => void;
  /** Clear the banner once the agent's uptime proves it rebooted after we
   *  flagged the change. `uptimeSeconds` comes from the GS status composite. */
  maybeAutoClearReboot: (uptimeSeconds: number | null | undefined) => void;
}

export const useConfigStore = create<ConfigState>((set, get) => ({
  config: null,
  ready: false,
  stale: false,
  error: null,
  pendingRebootPaths: loadPaths(),
  pendingRebootSince: loadSince(),

  refresh: async () => {
    try {
      const config = await getConfig();
      set({ config, ready: true, stale: false, error: null });
    } catch (err) {
      set((prev) => ({
        ready: true,
        stale: true,
        error: err instanceof Error ? err.message : String(err),
        config: prev.config,
      }));
    }
  },

  write: async (key, value) => {
    try {
      const body = await apiFetch<Record<string, unknown>>("/api/config", {
        method: "PUT",
        body: { key, value },
      });
      // The route returns a 200 with an `error` field for a bad key / value cast.
      if (body && typeof body === "object" && typeof body.error === "string") {
        return { ok: false, error: body.error };
      }
      // Reflect the persisted value.
      await get().refresh();
      return {
        ok: true,
        persisted: typeof body?.persisted === "boolean" ? body.persisted : undefined,
        value: body?.value,
      };
    } catch (err) {
      if (err instanceof ApiError) {
        const detail = (err.body as ConfigErrorBody | undefined)?.detail?.error;
        const msg =
          detail?.messages?.join("; ") ?? detail?.message ?? err.message;
        return { ok: false, error: msg };
      }
      return { ok: false, error: err instanceof Error ? err.message : String(err) };
    }
  },

  clearReboot: () => {
    persistReboot([], null);
    set({ pendingRebootPaths: [], pendingRebootSince: null });
  },

  maybeAutoClearReboot: (uptimeSeconds) => {
    const { pendingRebootSince, pendingRebootPaths } = get();
    if (!pendingRebootPaths.length || pendingRebootSince == null) return;
    if (uptimeSeconds == null || !Number.isFinite(uptimeSeconds)) return;
    const bootTimeMs = Date.now() - uptimeSeconds * 1000;
    if (bootTimeMs > pendingRebootSince + REBOOT_CLEAR_MARGIN_MS) {
      get().clearReboot();
    }
  },
}));

/** Record that `dotpath` was written and needs a reboot to take effect. Kept
 *  outside the store object so a caller (the editor) can flag without threading
 *  the setter through props. */
export function markRebootPending(dotpath: string): void {
  const { pendingRebootPaths, pendingRebootSince } = useConfigStore.getState();
  if (pendingRebootPaths.includes(dotpath)) return;
  const paths = [...pendingRebootPaths, dotpath];
  const since = pendingRebootSince ?? Date.now();
  persistReboot(paths, since);
  useConfigStore.setState({ pendingRebootPaths: paths, pendingRebootSince: since });
}

// ── ref-counted background poll ──────────────────────────────────────────────

let refCount = 0;
let pollTimer: ReturnType<typeof setInterval> | null = null;

/** Subscribe a component to the shared config: the first subscriber loads the
 *  config and starts a gentle background poll; the last one stops it. Only one
 *  Settings drill screen is mounted at a time, so this loads once on entry and
 *  keeps the snapshot warm through the drill. */
export function useConfigSubscription(): void {
  useEffect(() => {
    refCount += 1;
    if (refCount === 1) {
      void useConfigStore.getState().refresh();
      pollTimer = setInterval(() => {
        if (typeof document === "undefined" || !document.hidden) {
          void useConfigStore.getState().refresh();
        }
      }, POLL_MS);
    }
    return () => {
      refCount -= 1;
      if (refCount <= 0) {
        refCount = 0;
        if (pollTimer) {
          clearInterval(pollTimer);
          pollTimer = null;
        }
      }
    };
  }, []);
}
