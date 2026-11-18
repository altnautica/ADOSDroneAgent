// The WHEP receiver's jitter-buffer floor, chosen from what the receiver
// measures.
//
// `RTCRtpReceiver.jitterBufferTarget` is a floor under the browser's own
// adaptive buffer: 0 (or unset) leaves the adaptive buffer alone, which on a
// clean link holds about 15 ms. A session therefore starts with no floor, the
// lowest delay the link allows, and this controller moves the floor one rung
// of a coarse ladder at a time, once a second on the transport's stats poll:
// - Up on observed harm: a freeze the decoder reported, RTP interarrival
//   jitter above a frame period, or packet loss above 2 %. A buffer shorter
//   than the recovery interval turns recoverable loss into a visible freeze,
//   and the adaptive buffer alone grows only after the freeze.
// - Down after a sustained clean run: ten clean windows and 15 s since the
//   last change, so one quiet second between freezes never undoes a step,
//   back to no floor at all.
// - Never faster than one change per 4 s: every resize costs a resync, and a
//   buffer that keeps resizing is worse than either depth.

import { readInboundVideo, type InboundVideoCounters } from "./video-stats";

/** Floor depths in ms, ascending: none, then ~3, ~5 and ~7 frame periods at
 *  30 fps, the range over which a loss burst can be concealed without the
 *  delay becoming the dominant term. A session starts at rung 0. */
export const JITTER_TARGET_LADDER_MS: readonly number[] = [0, 100, 160, 220];

/** `RTCRtpReceiver.jitterBufferTarget`'s accepted range; outside it the
 *  setter throws, so every target is clamped. */
export const JITTER_TARGET_MIN_MS = 0;
export const JITTER_TARGET_MAX_MS = 4000;

/** The minimum time between two changes, the floor for a step up. */
export const MIN_DWELL_MS = 4000;

/** The minimum time since the last change before a step down: a step up
 *  answers a freeze the operator saw, a step down only saves a few frames. */
export const RELAX_DWELL_MS = 15_000;

/** RTP interarrival jitter at or above this is harm (just over one frame
 *  period at 30 fps); below the relax level the link is quiet. */
export const JITTER_ESCALATE_MS = 30;
export const JITTER_RELAX_MS = 15;

/** Loss fractions for the two directions. */
export const LOSS_ESCALATE = 0.02;
export const LOSS_RELAX = 0.005;

/** Consecutive clean windows (one per stats poll) before a step down. */
export const CLEAN_WINDOWS_TO_RELAX = 10;

/** One poll window's receiver measurements. */
export interface JitterSample {
  /** Freezes the decoder reported in this window (`freezeCount` delta). */
  freezeDelta: number;
  /** RTP interarrival jitter in ms (`inbound-rtp.jitter` × 1000). */
  rtpJitterMs: number;
  /** Lost / (lost + received) over this window; negative or NaN reads 0. */
  lossFraction: number;
  /** Timestamp for the dwell gates. */
  nowMs: number;
}

export interface JitterControllerState {
  /** Index into {@link JITTER_TARGET_LADDER_MS}. */
  rung: number;
  /** When the rung last moved; null before it ever has, so the first move is
   *  free. Not 0: a monotonic or faked clock can read 0, and the dwell gate
   *  must not silently disengage then. */
  lastChangeAtMs: number | null;
  /** Consecutive clean windows since the last one that was not. */
  cleanWindows: number;
}

export interface JitterDecision extends JitterControllerState {
  /** The clamped depth to apply, in ms. */
  targetMs: number;
  /** True when `targetMs` differs from the target before this sample. */
  changed: boolean;
  /** Why the rung moved. */
  reason: "freeze" | "jitter" | "loss" | "sustained-clean" | null;
}

/** A fresh session's state: no floor, as the receiver starts. */
export function initialJitterState(): JitterControllerState {
  return { rung: 0, lastChangeAtMs: null, cleanWindows: 0 };
}

/** The ladder depth for a rung, clamped into the accepted range. */
export function jitterTargetForRung(rung: number): number {
  const idx = Math.min(Math.max(rung, 0), JITTER_TARGET_LADDER_MS.length - 1);
  return Math.min(Math.max(JITTER_TARGET_LADDER_MS[idx], JITTER_TARGET_MIN_MS), JITTER_TARGET_MAX_MS);
}

function normalizedLoss(value: number): number {
  if (!Number.isFinite(value) || value <= 0) return 0;
  return Math.min(value, 1);
}

/** One control step. Pure, so the hysteresis is testable without a browser. */
export function nextJitterTarget(state: JitterControllerState, sample: JitterSample): JitterDecision {
  const previousTarget = jitterTargetForRung(state.rung);
  const jitterMs = Number.isFinite(sample.rtpJitterMs) ? Math.max(sample.rtpJitterMs, 0) : 0;
  const loss = normalizedLoss(sample.lossFraction);
  const freezes = Number.isFinite(sample.freezeDelta) ? Math.max(sample.freezeDelta, 0) : 0;

  const harm: JitterDecision["reason"] =
    freezes > 0 ? "freeze" : jitterMs >= JITTER_ESCALATE_MS ? "jitter" : loss >= LOSS_ESCALATE ? "loss" : null;
  const clean = freezes === 0 && jitterMs < JITTER_RELAX_MS && loss < LOSS_RELAX;
  const cleanWindows = clean ? state.cleanWindows + 1 : 0;

  // The dwell gates the change, not the measurement: harm during the dwell
  // still resets `cleanWindows`, so a bad link escalates on the next
  // eligible window.
  const sinceChange = state.lastChangeAtMs === null ? Number.POSITIVE_INFINITY : sample.nowMs - state.lastChangeAtMs;
  const atTop = state.rung >= JITTER_TARGET_LADDER_MS.length - 1;
  const atBottom = state.rung <= 0;

  const move = (rung: number, reason: JitterDecision["reason"]): JitterDecision => ({
    rung,
    lastChangeAtMs: sample.nowMs,
    cleanWindows: 0,
    targetMs: jitterTargetForRung(rung),
    changed: jitterTargetForRung(rung) !== previousTarget,
    reason,
  });
  if (harm && sinceChange >= MIN_DWELL_MS && !atTop) return move(state.rung + 1, harm);
  if (!harm && cleanWindows >= CLEAN_WINDOWS_TO_RELAX && sinceChange >= RELAX_DWELL_MS && !atBottom) {
    return move(state.rung - 1, "sustained-clean");
  }
  return { rung: state.rung, lastChangeAtMs: state.lastChangeAtMs, cleanWindows, targetMs: previousTarget, changed: false, reason: null };
}

/** The receiver's latency knobs: `jitterBufferTarget` in milliseconds (the
 *  spec's), `playoutDelayHint` in seconds (Chromium's older one). Optional
 *  because neither exists on every browser. */
interface LatencyTunableReceiver {
  playoutDelayHint?: number | null;
  jitterBufferTarget?: number | null;
}

/** Apply `targetMs` to every video receiver on the connection. Returns how
 *  many receivers took it, so a browser with neither knob reports 0. */
export function applyJitterTarget(pc: Pick<RTCPeerConnection, "getReceivers"> | null, targetMs: number): number {
  if (!pc) return 0;
  const clamped = Math.min(Math.max(targetMs, JITTER_TARGET_MIN_MS), JITTER_TARGET_MAX_MS);
  let receivers: RTCRtpReceiver[];
  try {
    receivers = pc.getReceivers();
  } catch {
    return 0;
  }
  let applied = 0;
  for (const receiver of receivers) {
    if (receiver.track && receiver.track.kind !== "video") continue;
    const tunable = receiver as unknown as LatencyTunableReceiver;
    let touched = false;
    try {
      if ("jitterBufferTarget" in tunable) {
        tunable.jitterBufferTarget = clamped;
        touched = true;
      }
      if ("playoutDelayHint" in tunable) {
        tunable.playoutDelayHint = clamped / 1000;
        touched = true;
      }
    } catch {
      // Exposed but the value was refused: leave the receiver as it was.
      touched = false;
    }
    if (touched) applied += 1;
  }
  return applied;
}

/** The controller's sample from two consecutive counter readings: counters
 *  that went backwards (a new receiver) read as no change. */
export function jitterSampleBetween(prev: InboundVideoCounters, cur: InboundVideoCounters, nowMs: number): JitterSample {
  const lost = Math.max(cur.packetsLost - prev.packetsLost, 0);
  const received = Math.max(cur.packetsReceived - prev.packetsReceived, 0);
  return {
    freezeDelta: Math.max(cur.freezeCount - prev.freezeCount, 0),
    rtpJitterMs: cur.jitter * 1000,
    lossFraction: lost + received === 0 ? 0 : lost / (lost + received),
    nowMs,
  };
}

/** A counter reader for one WHEP session that also runs the controller on
 *  every reading, retuning the receiver when the decision changes the depth. */
export function jitterTunedReader(
  pc: Pick<RTCPeerConnection, "getStats" | "getReceivers">,
  now: () => number = () => performance.now(),
): () => Promise<InboundVideoCounters | null> {
  let state = initialJitterState();
  let prev: InboundVideoCounters | null = null;
  return async () => {
    const cur = await readInboundVideo(pc);
    if (prev && cur) {
      const decision = nextJitterTarget(state, jitterSampleBetween(prev, cur, now()));
      state = decision;
      if (decision.changed) applyJitterTarget(pc, decision.targetMs);
    }
    prev = cur;
    return cur;
  };
}
