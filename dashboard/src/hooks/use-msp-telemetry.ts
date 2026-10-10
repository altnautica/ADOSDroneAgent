/**
 * React hook: live MSP telemetry from a Betaflight/iNav flight controller over
 * the agent's transparent `ws://<host>:8765/` proxy. The agent decodes zero MSP
 * telemetry (it is a byte-pipe), so the browser runs the MSP poller itself.
 *
 * Returns null until a session is requested (firmware === null) and while the
 * first snapshot is being taken; otherwise a rolling snapshot flushed at ~5 Hz.
 * The session is closed while the tab is hidden and reopened when it is shown.
 *
 * @module hooks/use-msp-telemetry
 */

import { useEffect, useState } from "react";

/** Fixed pause before a dropped or failed MSP session is redialled. */
const MSP_RECONNECT_MS = 3000;

import type { MspVariant } from "@/lib/fc-firmware";
import {
  MspTelemetryClient,
  type MspTelemetrySnapshot,
} from "@/lib/msp/msp-telemetry-poller";

/** True while the tab is visible; flips on `visibilitychange`. */
function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => !document.hidden);
  useEffect(() => {
    const onChange = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onChange);
    return () => document.removeEventListener("visibilitychange", onChange);
  }, []);
  return visible;
}

export function useMspTelemetry(firmware: MspVariant | null): MspTelemetrySnapshot | null {
  const [snap, setSnap] = useState<MspTelemetrySnapshot | null>(null);
  // Bumped to redial after the session closes or fails to open.
  const [attempt, setAttempt] = useState(0);
  // A hidden tab closes the MSP session (no polling the FC for nobody) and
  // reopens it when the tab is shown again; the last snapshot stays on screen.
  const visible = usePageVisible();

  useEffect(() => {
    if (!firmware) {
      setSnap(null);
      return;
    }
    if (!visible) return;
    let cancelled = false;
    let redial: ReturnType<typeof setTimeout> | null = null;
    const ac = new AbortController();
    const client = new MspTelemetryClient(firmware);

    void client.connect(ac.signal).catch(() => {
      // A connect failure is recorded in the client snapshot (linkState/error)
      // and surfaces on the next flush, which schedules the redial.
    });

    // Decode runs faster than we want to re-render; flush the rolling snapshot
    // to React at ~5 Hz so the UI stays smooth without thrashing. A closed or
    // failed session is redialled on a fixed cadence with no cap — the view
    // says "reconnecting", so it must actually reconnect.
    const flush = setInterval(() => {
      if (cancelled) return;
      const s = client.snapshot();
      setSnap(s);
      if ((s.linkState === "closed" || s.linkState === "error") && redial === null) {
        redial = setTimeout(() => {
          if (!cancelled) setAttempt((n) => n + 1);
        }, MSP_RECONNECT_MS);
      }
    }, 200);

    return () => {
      cancelled = true;
      clearInterval(flush);
      if (redial !== null) clearTimeout(redial);
      ac.abort();
      void client.disconnect();
    };
  }, [firmware, attempt, visible]);

  return snap;
}
