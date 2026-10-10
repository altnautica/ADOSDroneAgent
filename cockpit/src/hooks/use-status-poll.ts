// The shell's status poll, mounted once. Request budget per second:
//   drone          1 × /api/status/full (+ /api/wfb every 10 s for the link
//                  diagnosis the consolidated body does not carry)
//   ground station 1 × /api/v1/ground-station/status
//   both           /api/v1/battery every 5 s when the agent advertises
//                  `battery.health`
// Polls pause while the page is hidden and back off while the node refuses
// requests. A failed poll keeps the last snapshot and flips `stale`.

import { useEffect } from "react";

import {
  droneStatusFromFull,
  getBatteryHealth,
  getCapabilities,
  getGsStatus,
  getLinkDiagnosis,
  getStatusFull,
  type LinkDiagnosis,
} from "@/lib/api";
import { pollIntervalMs, renderProfile } from "@/lib/render-profile";
import { ApiError } from "@/shared/api-fetch";
import { useProfile } from "@/shared/use-profile";
import { pollIntervalFor, useReachStore } from "@/stores/reach-store";
import { useStatusStore } from "@/stores/status-store";

export const STATUS_POLL_MS = 1000;
export const LINK_DIAG_POLL_MS = 10_000;
export const BATTERY_POLL_MS = 5000;

/** A self-scheduling loop that pauses while the page is hidden. Returns stop. */
export function pollLoop(
  run: (signal: AbortSignal) => Promise<void>,
  intervalMs: () => number,
): () => void {
  let cancelled = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const controller = new AbortController();
  const hidden = () => typeof document !== "undefined" && document.hidden;

  const tick = async () => {
    if (cancelled || hidden()) return;
    try {
      await run(controller.signal);
    } catch {
      // `run` records its own failure; the loop keeps its cadence.
    }
    if (!cancelled) timer = setTimeout(tick, intervalMs());
  };
  const onVisibility = () => {
    if (!hidden()) {
      clearTimeout(timer);
      void tick();
    }
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

export function useStatusPoll(): void {
  const profile = useProfile();

  useEffect(() => {
    if (profile === null) return;
    const base = pollIntervalMs(STATUS_POLL_MS, renderProfile());
    let diag: LinkDiagnosis | null = null;
    let diagAt = -Infinity;

    const stopStatus = pollLoop(
      async (signal) => {
        try {
          let status;
          if (profile === "drone") {
            if (performance.now() - diagAt >= LINK_DIAG_POLL_MS) {
              diagAt = performance.now();
              diag = await getLinkDiagnosis(signal).catch(() => diag);
            }
            status = droneStatusFromFull(await getStatusFull(signal), diag);
          } else {
            status = await getGsStatus(signal);
          }
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
