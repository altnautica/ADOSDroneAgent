// The shell's status poll, mounted once. Request budget per second:
//   drone          1 × /api/status/full (+ /api/wfb every 10 s for the link
//                  diagnosis the consolidated body does not carry)
//   ground station 1 × /api/v1/ground-station/status (+ /api/status every 5 s
//                  for the node's own FC link, which the composite does not
//                  carry and the skill gate reads)
//   both           /api/v1/battery every 5 s when the agent advertises
//                  `battery.health`
// Polls pause while the page is hidden and back off while the node refuses
// requests. A failed poll keeps the last snapshot and flips `stale`.

import { useEffect } from "react";

import {
  droneStatusFromFull,
  getBatteryHealth,
  getCapabilities,
  getFcConnected,
  getGsStatus,
  getLinkDiagnosis,
  getStatusFull,
  type LinkDiagnosis,
} from "@/lib/api";
import type { GsStatus } from "@/lib/types";
import { pollIntervalMs, renderProfile } from "@/lib/render-profile";
import { ApiError } from "@/shared/api-fetch";
import { useProfile, type AgentProfile } from "@/shared/use-profile";
import { pollIntervalFor, useReachStore } from "@/stores/reach-store";
import { useStatusStore } from "@/stores/status-store";

export const STATUS_POLL_MS = 1000;
export const LINK_DIAG_POLL_MS = 10_000;
export const BATTERY_POLL_MS = 5000;
export const FC_LINK_POLL_MS = 5000;

/** A self-scheduling loop that pauses while the page is hidden. Returns stop. */
export function pollLoop(
  run: (signal: AbortSignal) => Promise<void>,
  intervalMs: () => number,
): () => void {
  let cancelled = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const controller = new AbortController();
  const hidden = () => typeof document !== "undefined" && document.hidden;

  let inFlight = false;
  let pending = false;

  const tick = async () => {
    pending = false;
    if (cancelled || hidden()) return;
    inFlight = true;
    try {
      await run(controller.signal);
    } catch {
      // `run` records its own failure; the loop keeps its cadence.
    }
    inFlight = false;
    if (!cancelled) {
      pending = true;
      timer = setTimeout(tick, intervalMs());
    }
  };
  // Resume a loop the hidden page paused. A request in flight or a tick
  // already scheduled keeps the one chain; a quick hide/show never forks it.
  const onVisibility = () => {
    if (!hidden() && !inFlight && !pending) void tick();
  };

  void tick();
  if (typeof document !== "undefined") document.addEventListener("visibilitychange", onVisibility);
  return () => {
    cancelled = true;
    controller.abort();
    clearTimeout(timer);
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", onVisibility);
    }
  };
}

/** The status read for one tick of `profile`'s poll. The returned reader holds
 *  the slower secondary reads between ticks; it throws when the primary read
 *  fails, so the caller can flip `stale`. */
export function statusReader(profile: AgentProfile): (signal: AbortSignal) => Promise<GsStatus> {
  let diag: LinkDiagnosis | null = null;
  let diagAt = -Infinity;
  let fcConnected = false;
  let fcAt = -Infinity;

  return async (signal) => {
    if (profile === "drone") {
      if (performance.now() - diagAt >= LINK_DIAG_POLL_MS) {
        diagAt = performance.now();
        diag = await getLinkDiagnosis(signal).catch(() => diag);
      }
      return droneStatusFromFull(await getStatusFull(signal), diag);
    }
    // A ground station's router may hold a MAVLink link of its own (the same
    // `fc_connected` `/api/command` gates on); `/api/status` reports it on
    // every profile. A failed read is no link, never the last guess.
    if (performance.now() - fcAt >= FC_LINK_POLL_MS) {
      fcAt = performance.now();
      fcConnected = await getFcConnected(signal).catch(() => false);
    }
    return { ...(await getGsStatus(signal)), fc_connected: fcConnected };
  };
}

export function useStatusPoll(): void {
  const profile = useProfile();

  useEffect(() => {
    if (profile === null) return;
    const base = pollIntervalMs(STATUS_POLL_MS, renderProfile());
    const readStatus = statusReader(profile);

    const stopStatus = pollLoop(
      async (signal) => {
        try {
          const status = await readStatus(signal);
          useReachStore.getState().report(null, true);
          useStatusStore.setState({ status, stale: false, error: null });
        } catch (err) {
          if (signal.aborted) return;
          useReachStore.getState().report(err instanceof ApiError ? err.status : null, false);
          useStatusStore.setState({
            stale: true,
            error: err instanceof Error ? err.message : String(err),
          });
        }
      },
      () => pollIntervalFor(base, useReachStore.getState().refusal),
    );

    let stopBattery: (() => void) | null = null;
    let cancelled = false;
    void getCapabilities()
      .then((caps) => {
        if (cancelled || !caps.includes("battery.health")) return;
        stopBattery = pollLoop(
          async (signal) => {
            try {
              useStatusStore.setState({ battery: await getBatteryHealth(signal) });
            } catch {
              if (!signal.aborted) useStatusStore.setState({ battery: null });
            }
          },
          () => pollIntervalFor(BATTERY_POLL_MS, useReachStore.getState().refusal),
        );
      })
      .catch(() => undefined);

    return () => {
      cancelled = true;
      stopStatus();
      stopBattery?.();
    };
  }, [profile]);
}
