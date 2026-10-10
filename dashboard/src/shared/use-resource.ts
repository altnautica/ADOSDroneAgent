// A self-scheduling poll for one agent read. On a failed poll it keeps the
// last snapshot and flips `stale`, so a surface dims honestly rather than
// blanking or fabricating. A slow poll never overlaps the next, polling pauses
// while the document is hidden, and `refresh()` fires an immediate re-poll
// (after a write, so the screen shows the agent's real state).

import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError } from "./api-fetch";

export interface Resource<T> {
  data: T | null;
  error: string | null;
  /** True once at least one poll has settled (success or failure). */
  ready: boolean;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
  /** HTTP status of the last failure when it was an ApiError, else null. */
  status: number | null;
  refresh: () => void;
}

export interface ResourceHooks {
  /** Told about every poll outcome (status null on success or a network fault). */
  report?: (status: number | null, ok: boolean) => void;
  /** Maps the requested interval to the one actually used (e.g. back off
   *  while the node refuses requests). */
  intervalFor?: (baseMs: number) => number;
}

let hooks: ResourceHooks = {};

/** App-level wiring for every `useResource` poll. */
export function configureResource(next: ResourceHooks): void {
  hooks = next;
}

export interface ResourceOptions {
  enabled?: boolean;
  pauseWhenHidden?: boolean;
}

export function useResource<T>(
  fetcher: (signal: AbortSignal) => Promise<T>,
  intervalMs = 1500,
  { enabled = true, pauseWhenHidden = true }: ResourceOptions = {},
): Resource<T> {
  const [state, setState] = useState<Omit<Resource<T>, "refresh">>({
    data: null,
    error: null,
    ready: false,
    stale: false,
    status: null,
  });
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;
  const [nonce, setNonce] = useState(0);
  const refresh = useCallback(() => setNonce((n) => n + 1), []);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;

    const hidden = () =>
      pauseWhenHidden && typeof document !== "undefined" && document.hidden;

    const schedule = () => {
      if (cancelled) return;
      const wait = hooks.intervalFor ? hooks.intervalFor(intervalMs) : intervalMs;
      timer = setTimeout(tick, wait);
    };

    const tick = async () => {
      if (hidden()) return; // resumed by the visibility listener
      try {
        const data = await fetcherRef.current(controller.signal);
        if (cancelled) return;
        hooks.report?.(null, true);
        setState({ data, error: null, ready: true, stale: false, status: null });
      } catch (err) {
        if (cancelled || controller.signal.aborted) return;
        const status = err instanceof ApiError ? err.status : null;
        hooks.report?.(status, false);
        setState((prev) => ({
          data: prev.data,
          error: err instanceof Error ? err.message : String(err),
          ready: true,
          stale: true,
          status,
        }));
      }
      schedule();
    };

    const onVisibility = () => {
      if (!document.hidden) {
        clearTimeout(timer);
        void tick();
      }
    };

    void tick();
    if (pauseWhenHidden && typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisibility);
    }
    return () => {
      cancelled = true;
      controller.abort();
      clearTimeout(timer);
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibility);
      }
    };
  }, [intervalMs, nonce, enabled, pauseWhenHidden]);

  return { ...state, refresh };
}
