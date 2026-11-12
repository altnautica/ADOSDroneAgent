// A restart the operator asked for (board reboot, profile apply). While it is
// pending the dashboard says "Rebooting…" instead of "Agent unreachable", and it
// knows the agent is back once the heartbeat answers after the restart.

/** Past this, the restart is reported as not coming back. */
export const RESTART_TIMEOUT_MS = 120_000;

export type RestartKind = "reboot" | "profile";

export interface RestartPending {
  kind: RestartKind;
  /** When the restart was asked for (epoch ms). */
  since: number;
  /** The agent uptime read just before it, when known. */
  baselineUptimeS: number | null;
  /** The heartbeat failed at least once since. */
  sawDown: boolean;
}

export type RestartPhase = "waiting" | "back" | "timedOut";

/** Where a pending restart stands. The agent is back once it answers after
 *  having gone silent, or with an uptime younger than the request (a restart
 *  between two polls). Until then it waits, and after the timeout it gives up. */
export function restartPhase(
  p: RestartPending,
  now: number,
  heartbeat: { reachable: boolean; uptimeS: number | null },
): RestartPhase {
  if (heartbeat.reachable && heartbeat.uptimeS != null) {
    const elapsedS = (now - p.since) / 1000;
    const restarted =
      p.sawDown ||
      heartbeat.uptimeS < elapsedS ||
      (p.baselineUptimeS != null && heartbeat.uptimeS < p.baselineUptimeS);
    if (restarted) return "back";
  }
  return now - p.since > RESTART_TIMEOUT_MS ? "timedOut" : "waiting";
}

/** Whole seconds since the request. */
export function restartElapsedS(p: RestartPending, now: number): number {
  return Math.max(0, Math.floor((now - p.since) / 1000));
}
