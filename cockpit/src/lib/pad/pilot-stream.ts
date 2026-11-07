// The stick stream's start/stop rules (C4), shared by the controller stream
// and the on-screen sticks: stream `input` at 50 Hz while the drone flies in
// `ados` control mode and a live source is in hand; the moment the pad
// disconnects, the tab is hidden or the pad's timestamp goes stale for more
// than 150 ms, stop and send one `release` so the pilot brakes to a hold.

import { STALE_SAMPLE_MS } from "@/lib/pad/ds4-map";
import type { PilotAxes } from "@/lib/pilot-link";

export type StopReason = "disabled" | "disconnected" | "hidden" | "stale";

export interface StreamTick {
  /** Same clock as the sample timestamp (`performance.now()`). */
  now: number;
  /** Control mode `ados`, this cockpit's own drone, no native stick owner. */
  enabled: boolean;
  connected: boolean;
  hidden: boolean;
  /** The source's last report time, or null when none yet. */
  sampleAt: number | null;
  axes: PilotAxes;
  buttons: number;
}

export function stopReason(t: StreamTick): StopReason | null {
  if (!t.enabled) return "disabled";
  if (!t.connected) return "disconnected";
  if (t.hidden) return "hidden";
  if (t.sampleAt === null || t.now - t.sampleAt > STALE_SAMPLE_MS) return "stale";
  return null;
}

export interface StreamIo {
  send: (axes: PilotAxes, buttons: number) => boolean;
  release: () => void;
}

/** One stream's state: sends while the rules allow, releases once on stop. */
export class PilotStreamer {
  private streaming = false;
  private readonly io: StreamIo;

  constructor(io: StreamIo) {
    this.io = io;
  }

  get active(): boolean {
    return this.streaming;
  }

  /** One 20 ms tick. Returns why the stream is stopped, or null while it
   *  streams. */
  tick(t: StreamTick): StopReason | null {
    const reason = stopReason(t);
    if (reason) {
      this.stop();
      return reason;
    }
    this.streaming = this.io.send(t.axes, t.buttons) || this.streaming;
    return null;
  }

  /** Stop now (unmount, toggle off); releases only if it was streaming. */
  stop(): void {
    if (!this.streaming) return;
    this.streaming = false;
    this.io.release();
  }
}
