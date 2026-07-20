// Polls the paired drone's live vehicle state (`GET /api/telemetry`) at ~5 Hz
// while the Feed is on screen and hands it to the flight instruments. It derives
// a `live` flag from attitude presence plus the message freshness (on-box the
// panel shares the agent's clock, so the ISO stamp gates freshness reliably),
// so the HUD draws a real horizon only when there is real attitude — never a
// fabricated level horizon when the link is silent. A failed poll flips `stale`
// and keeps the last snapshot rather than blanking.

import { useEffect, useRef, useState } from "react";

import { getTelemetry } from "@/lib/api";
import type { VehicleState } from "@/lib/types";

export interface FlightTelemetryState {
  telemetry: VehicleState | null;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
  /** True when the snapshot carries fresh attitude from a live link. Gates the
   *  artificial horizon so it never shows a fabricated level attitude. */
  live: boolean;
}

/** How recent the last message must be for the attitude to count as live. */
const LIVE_FRESH_MS = 4000;

function isLive(t: VehicleState | null): boolean {
  if (!t) return false;
  const att = t.attitude;
  const hasAttitude =
    att != null && Number.isFinite(att.roll) && Number.isFinite(att.pitch);
  if (!hasAttitude) return false;
  const stamp = t.last_update ?? t.last_heartbeat;
  const parsed = stamp ? Date.parse(stamp) : NaN;
  if (!Number.isFinite(parsed)) return false;
  return Date.now() - parsed < LIVE_FRESH_MS;
}

/** Poll `/api/telemetry` every `intervalMs` (default 200 ms ≈ 5 Hz). */
export function useFlightTelemetry(intervalMs = 200): FlightTelemetryState {
  const [state, setState] = useState<FlightTelemetryState>({
    telemetry: null,
    stale: false,
    live: false,
  });
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();

    const tick = async () => {
      try {
        const telemetry = await getTelemetry(controller.signal);
        if (cancelled) return;
        setState({ telemetry, stale: false, live: isLive(telemetry) });
      } catch {
        if (cancelled || controller.signal.aborted) return;
        setState((prev) => ({
          telemetry: prev.telemetry,
          stale: true,
          live: false,
        }));
      } finally {
        if (!cancelled) {
          timer.current = setTimeout(tick, intervalMs);
        }
      }
    };

    void tick();

    return () => {
      cancelled = true;
      controller.abort();
      if (timer.current) clearTimeout(timer.current);
    };
  }, [intervalMs]);

  return state;
}
