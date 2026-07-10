/**
 * React hook: live MSP telemetry from a Betaflight/iNav flight controller over
 * the agent's transparent `ws://<host>:8765/` proxy. The agent decodes zero MSP
 * telemetry (it is a byte-pipe), so the browser runs the MSP poller itself.
 *
 * Returns null until a session is requested (firmware === null) and while the
 * first snapshot is being taken; otherwise a rolling snapshot flushed at ~5 Hz.
 *
 * @module hooks/use-msp-telemetry
 */

import { useEffect, useState } from "react";

import type { MspVariant } from "@/lib/fc-firmware";
import {
  MspTelemetryClient,
  type MspTelemetrySnapshot,
} from "@/lib/msp/msp-telemetry-poller";

export function useMspTelemetry(firmware: MspVariant | null): MspTelemetrySnapshot | null {
  const [snap, setSnap] = useState<MspTelemetrySnapshot | null>(null);

  useEffect(() => {
    if (!firmware) {
      setSnap(null);
      return;
    }
    let cancelled = false;
    const ac = new AbortController();
    const client = new MspTelemetryClient(firmware);
    setSnap(null);

    void client.connect(ac.signal).catch(() => {
      // A connect failure is recorded in the client snapshot (linkState/error)
      // and surfaces on the next flush; nothing to do here.
    });

    // Decode runs faster than we want to re-render; flush the rolling snapshot
    // to React at ~5 Hz so the UI stays smooth without thrashing.
    const flush = setInterval(() => {
      if (!cancelled) setSnap(client.snapshot());
    }, 200);

    return () => {
      cancelled = true;
      clearInterval(flush);
      ac.abort();
      void client.disconnect();
    };
  }, [firmware]);

  return snap;
}
