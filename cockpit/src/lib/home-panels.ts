// Pure rules behind the System screen's node cards (ported from the dashboard
// home): the sparkline ring, the FC card's state, the cloud relay's heartbeat
// wording and the video pipeline's idle reason. Kept out of React so each rule
// is testable on its own.

import { mspVariant, type MspVariant } from "@/shared/api/fc-firmware";
import type { FcSnapshot } from "@/shared/api/types";

/** One sparkline sample, keyed by epoch seconds. */
export interface TimePoint {
  time: number;
  value: number;
}

/** Push `value` at `timeS` onto a bounded ring, one sample per second (a second
 *  sample in the same second replaces the first). A null or NaN value leaves
 *  the ring as it is: a gap is not a zero. Returns a new array. */
export function pushSample(ring: readonly TimePoint[], timeS: number, value: number | null | undefined, maxPoints = 60): TimePoint[] {
  if (value == null || Number.isNaN(value)) return ring as TimePoint[];
  const last = ring[ring.length - 1];
  if (last && last.time === timeS) return [...ring.slice(0, -1), { time: timeS, value }];
  const next = [...ring, { time: timeS, value }];
  return next.length > maxPoints ? next.slice(next.length - maxPoints) : next;
}

/** SVG polyline points for a sparkline in a `width`×`height` box. A flat series
 *  draws a mid-height line; fewer than two points draw nothing. */
export function sparkPoints(points: readonly TimePoint[], width: number, height: number): string {
  if (points.length < 2) return "";
  const values = points.map((p) => p.value);
  const min = Math.min(...values);
  const span = Math.max(...values) - min;
  const step = width / (points.length - 1);
  return points
    .map((p, i) => {
      const y = span === 0 ? height / 2 : height - ((p.value - min) / span) * height;
      return `${(i * step).toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
}

/** What the FC card can honestly show.
 *  - `connecting`: no snapshot yet.
 *  - `msp`: a Betaflight/iNav FC identified over serial (no MAVLink heartbeat).
 *  - `absent`: no FC on the link.
 *  - `mavlink`: a MAVLink FC; `waiting` until its first heartbeat lands. */
export type FcView =
  | { kind: "connecting" }
  | { kind: "msp"; variant: MspVariant }
  | { kind: "absent" }
  | { kind: "mavlink"; waiting: boolean };

export function fcView(fc: FcSnapshot | null | undefined, hasSnapshot: boolean, fcVariant: string | null | undefined): FcView {
  if (!hasSnapshot) return { kind: "connecting" };
  const variant = mspVariant(fcVariant);
  if (!fc?.connected && variant) return { kind: "msp", variant };
  if (!fc?.connected) return { kind: "absent" };
  return { kind: "mavlink", waiting: fc.last_heartbeat == null };
}

/** `GET /api/cloud/link`; 404 while the relay is not reporting. */
export interface CloudLink {
  paired: boolean;
  cloud_url_set: boolean;
  broker_connected: boolean | null;
  last_heartbeat_ok_ms: number | null;
  last_heartbeat_status: number | null;
  last_heartbeat_error: string | null;
}

/** The status-POST line: the age of the last good POST, else the HTTP status
 *  it got, else "no answer" on a transport error, else "not sent". */
export function cloudHeartbeat(
  link: CloudLink,
  nowMs: number,
): { key: "heartbeatOk"; age: number } | { key: "heartbeatHttp"; status: number } | { key: "noAnswer" } | { key: "notSent" } {
  if (link.last_heartbeat_ok_ms !== null) {
    return { key: "heartbeatOk", age: Math.max(0, Math.round((nowMs - link.last_heartbeat_ok_ms) / 1000)) };
  }
  if (link.last_heartbeat_status !== null) return { key: "heartbeatHttp", status: link.last_heartbeat_status };
  return link.last_heartbeat_error ? { key: "noAnswer" } : { key: "notSent" };
}

/** Why the video pipeline shows no picture, as a `video.panel.*` key, or null
 *  while it is running. A ground station waits on the drone's WFB stream
 *  first; a drone on its camera, then on a stream URL. */
export function videoIdleReason(input: {
  pipelineState: string | null | undefined;
  isGround: boolean;
  wfbPackets: number | null | undefined;
  haveStreamUrl: boolean;
}): "waitingForWfb" | "noCamera" | "noStream" | "pipelineIdle" | null {
  if (input.isGround && !((input.wfbPackets ?? 0) > 0)) return "waitingForWfb";
  if (input.pipelineState === "running") return input.haveStreamUrl ? null : "noStream";
  if (input.pipelineState === "no_camera") return "noCamera";
  return input.haveStreamUrl ? "pipelineIdle" : "noStream";
}
