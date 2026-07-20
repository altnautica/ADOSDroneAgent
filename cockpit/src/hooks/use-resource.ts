// A generic self-scheduling poll for the per-screen agent reads that sit beside
// the shared GS-status composite. Mirrors `use-telemetry`: on a failed poll it
// keeps the last snapshot and flips `stale` so a surface dims honestly rather
// than blanks or fabricates. A slow poll never overlaps the next (the next tick
// is scheduled only after the current settles), and a manual `refresh()` fires
// an immediate re-poll (used after a write action so the screen reflects the
// agent's real state, not an optimistic one).

import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError } from "@/lib/api";

export interface Resource<T> {
  data: T | null;
  error: string | null;
  /** True once at least one poll has settled (success or failure). */
  ready: boolean;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
  /** HTTP status of the last failure, when it was an ApiError (e.g. 404 for a
   *  route unavailable on this profile), else null. */
  status: number | null;
  /** Fire an immediate re-poll (e.g. after a write). */
  refresh: () => void;
}

/** Poll `fetcher` every `intervalMs`. The fetcher is read through a ref so an
 *  inline arrow passed each render does not restart the loop; only `intervalMs`
 *  and a manual refresh drive re-scheduling. */
export function useResource<T>(
  fetcher: (signal: AbortSignal) => Promise<T>,
  intervalMs = 1500,
): Resource<T> {
  const [state, setState] = useState<{
    data: T | null;
    error: string | null;
    ready: boolean;
    stale: boolean;
    status: number | null;
  }>({ data: null, error: null, ready: false, stale: false, status: null });

  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  const [nonce, setNonce] = useState(0);
  const refresh = useCallback(() => setNonce((n) => n + 1), []);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | null = null;

    const tick = async () => {
      try {
        const data = await fetcherRef.current(controller.signal);
        if (cancelled) return;
        setState({ data, error: null, ready: true, stale: false, status: null });
      } catch (err) {
        if (cancelled || controller.signal.aborted) return;
        const status = err instanceof ApiError ? err.status : null;
        setState((prev) => ({
          data: prev.data,
          error: err instanceof Error ? err.message : String(err),
          ready: true,
          stale: true,
          status,
        }));
      } finally {
        if (!cancelled) {
          timer = setTimeout(tick, intervalMs);
        }
      }
    };

    void tick();

    return () => {
      cancelled = true;
      controller.abort();
      if (timer) clearTimeout(timer);
    };
  }, [intervalMs, nonce]);

  return { ...state, refresh };
}
