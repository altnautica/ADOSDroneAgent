// Polls the paired drone's live vehicle state (`GET /api/telemetry`) at ~5 Hz
// while the Feed is on screen and hands it to the flight instruments. It derives
// a `live` flag from attitude presence plus the message freshness (on-box the
// panel shares the agent's clock, so the ISO stamp gates freshness reliably),
// so the HUD draws a real horizon only when there is real attitude — never a
// fabricated level horizon when the link is silent. A failed poll flips `stale`
// and keeps the last snapshot rather than blanking.
//
// `live` and `commandable` are deliberately two flags, not one. They used to be
// the same value, which was correct only while the sole source of telemetry was
// a directly attached flight controller. A ground station relaying an aircraft
// over the radio has genuine attitude to draw but cannot send that aircraft a
// COMMAND_LONG from here, and collapsing the two meant the honest answer to the
// second question ("no, not from this node") also blanked the first — a dead
// horizon over a working link. Instruments read `live`; command affordances read
// `commandable`.

import { useEffect, useRef, useState } from "react";

import { pollIntervalMs, renderProfile } from "@/lib/render-profile";

import { getTelemetry } from "@/lib/api";
import type { VehicleState } from "@/lib/types";

export interface FlightTelemetryState {
  telemetry: VehicleState | null;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
  /** True when the snapshot carries fresh attitude, whatever its source. Gates
   *  the artificial horizon and the tapes so they never show a fabricated level
   *  attitude — but a relayed aircraft is a real one, so this is true for it. */
  live: boolean;
  /** True only when the vehicle is reachable for commands FROM THIS NODE, i.e.
   *  a directly attached flight controller. False for a relayed aircraft: its
   *  readings are real, but this node is not the one that flies it. */
  commandable: boolean;
  /** True when the readings came over the radio from another node rather than
   *  from a local flight controller. Drives the provenance badge. */
  relayed: boolean;
}

/** How long the vehicle's timestamp may sit unchanged before the reading stops
 *  counting as live. */
const LIVE_FRESH_MS = 4000;

/** Whether the snapshot carries usable attitude at all. */
export function hasUsableAttitude(t: VehicleState | null): boolean {
  const att = t?.attitude;
  return att != null && Number.isFinite(att.roll) && Number.isFinite(att.pitch);
}

/** The vehicle timestamp this snapshot carries, or null when it has none. */
export function vehicleStamp(t: VehicleState | null): string | null {
  return t?.last_update ?? t?.last_heartbeat ?? null;
}

/**
 * Whether the snapshot carries usable, recent attitude — regardless of whether
 * it came from an attached FC or across the radio.
 *
 * `msSinceStampMoved` is how long the vehicle's timestamp has sat UNCHANGED,
 * measured on the client's own monotonic clock. This deliberately does not
 * compare the agent's timestamp against `Date.now()`, which is what it used to
 * do: that made the whole HUD depend on the agent's wall clock agreeing with
 * the browser's to within four seconds. On-box that holds, because the panel
 * and the agent share a clock — but a viewer on another machine, or a box that
 * booted without NTP, blanked every instrument permanently while
 * `/api/telemetry` was returning perfect attitude, with nothing on screen to
 * say why. Measuring how long the stamp has been standing still is immune to
 * skew and still catches the case that matters (an agent repeating a frozen
 * snapshot). A failed poll is handled separately, by `stale`.
 *
 * A snapshot with no timestamp at all is trusted when it has attitude: the
 * agent only emits vehicle fields once it has decided they are fresh (see the
 * `mavlink_alive` and relayed-freshness gates in `routes/status.rs`), so the
 * client has no better information and must not invent staleness.
 */
export function isLive(
  t: VehicleState | null,
  msSinceStampMoved: number | null,
): boolean {
  if (!hasUsableAttitude(t)) return false;
  if (msSinceStampMoved == null) return true;
  return msSinceStampMoved < LIVE_FRESH_MS;
}

/** Whether the readings arrived over the radio rather than from a local FC.
 *
 *  The agent stamps this only on the relayed path, so an absent field means a
 *  direct link. Reading the stamp rather than inferring from the absence of
 *  other fields keeps the two cases explicit. */
export function isRelayed(t: VehicleState | null): boolean {
  return t?.telemetry_source === "relayed";
}

/** Poll `/api/telemetry` every `intervalMs` (default 200 ms ≈ 5 Hz). */
export function useFlightTelemetry(intervalMs = pollIntervalMs(200, renderProfile())): FlightTelemetryState {
  const [state, setState] = useState<FlightTelemetryState>({
    telemetry: null,
    stale: false,
    live: false,
    commandable: false,
    relayed: false,
  });
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // The last vehicle timestamp seen, and when (on the client's own monotonic
  // clock) it last moved. Refs rather than state: they feed the next poll's
  // freshness decision and must not themselves trigger a render.
  const lastStamp = useRef<string | null>(null);
  const lastStampMovedAt = useRef<number>(0);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();

    const tick = async () => {
      try {
        const telemetry = await getTelemetry(controller.signal);
        if (cancelled) return;

        const stamp = vehicleStamp(telemetry);
        const nowMs = performance.now();
        if (stamp !== lastStamp.current) {
          lastStamp.current = stamp;
          lastStampMovedAt.current = nowMs;
        }
        const msSinceStampMoved =
          stamp == null ? null : nowMs - lastStampMovedAt.current;

        const live = isLive(telemetry, msSinceStampMoved);
        const relayed = isRelayed(telemetry);
        setState({
          telemetry,
          stale: false,
          live,
          // A relayed aircraft is never commandable from this node, however
          // healthy its readings are.
          commandable: live && !relayed,
          relayed,
        });
      } catch {
        if (cancelled || controller.signal.aborted) return;
        setState((prev) => ({
          telemetry: prev.telemetry,
          stale: true,
          live: false,
          commandable: false,
          // Provenance survives a failed poll: the last snapshot is still on
          // screen, and mislabelling its origin while it is visible would be
          // worse than saying nothing.
          relayed: prev.relayed,
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
