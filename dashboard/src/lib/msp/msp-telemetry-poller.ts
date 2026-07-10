/**
 * Live MSP telemetry poller for the dashboard Sensors view.
 *
 * MSP is a request/response protocol (unlike MAVLink, which streams), so live
 * telemetry from a Betaflight/iNav FC means periodically requesting each command
 * and decoding the reply. The agent is a byte-pipe for MSP, so this runs
 * entirely in the browser over the transparent `ws://<host>:8765/` proxy.
 *
 * Two pieces:
 *   - `MspTelemetryPoller` — periodic request scheduler (fast/medium/slow
 *     groups) with adaptive backoff when the serial queue saturates.
 *   - `MspTelemetryClient` — owns one WS transport + parser + queue + poller,
 *     decodes replies into a rolling snapshot the UI reads.
 *
 * @module lib/msp/msp-telemetry-poller
 * @license GPL-3.0-only
 */

import type { MspVariant } from "@/lib/fc-firmware";

import { MspParser } from "./parser";
import { MspSerialQueue } from "./serial-queue";
import {
  MSP_CMD,
  decodeAltitude,
  decodeAnalog,
  decodeAttitude,
  decodeInavStatus,
  decodeRawGps,
  decodeRc,
  decodeSensorFlags,
  decodeStatusEx,
  type MspAltitude,
  type MspAnalog,
  type MspAttitude,
  type MspRawGps,
  type MspStatus,
  type SensorFlag,
} from "./telemetry-decoders";
import { WebSocketTransport, mavlinkWsUrl } from "./transport";
import { WS_TICKET_PROTOCOL, mintMavlinkWsTicket } from "./ws-ticket";

// ── Poll groups ────────────────────────────────────────────────

interface PollGroup {
  name: string;
  commands: number[];
  intervalMs: number;
}

/** Skip a tick when the queue is this deep to avoid saturating the serial link. */
const MAX_PENDING_BEFORE_SKIP = 5;

/**
 * Firmware-specific poll groups. Rates are gentle relative to the GCS cockpit —
 * the dashboard wants live-ish state, not a full flight HUD — and the adaptive
 * backoff protects a slow serial link. iNav reads MSP2_INAV_STATUS (arming
 * flags), Betaflight reads MSP_STATUS_EX (mode flags); both carry cpu/sensors.
 */
function pollGroups(firmware: MspVariant): PollGroup[] {
  const statusCmd =
    firmware === "inav" ? MSP_CMD.MSP2_INAV_STATUS : MSP_CMD.MSP_STATUS_EX;
  return [
    { name: "fast", commands: [MSP_CMD.MSP_ATTITUDE, MSP_CMD.MSP_RC], intervalMs: 100 },
    { name: "medium", commands: [MSP_CMD.MSP_ANALOG, statusCmd], intervalMs: 250 },
    {
      name: "slow",
      commands: [MSP_CMD.MSP_RAW_GPS, MSP_CMD.MSP_ALTITUDE],
      intervalMs: 1000,
    },
  ];
}

/** Periodic MSP request scheduler. Decoded replies flow to `onData`. */
export class MspTelemetryPoller {
  private timers: ReturnType<typeof setInterval>[] = [];
  private running = false;

  constructor(
    private readonly queue: MspSerialQueue,
    private readonly groups: PollGroup[],
    private readonly onData: (command: number, payload: Uint8Array) => void,
  ) {}

  start(): void {
    if (this.running) return;
    this.running = true;
    for (const group of this.groups) {
      const timer = setInterval(() => this.pollGroup(group), group.intervalMs);
      this.timers.push(timer);
    }
  }

  stop(): void {
    this.running = false;
    for (const t of this.timers) clearInterval(t);
    this.timers = [];
  }

  private pollGroup(group: PollGroup): void {
    // Adaptive backoff: skip this tick when the queue is backed up.
    if (this.queue.pending > MAX_PENDING_BEFORE_SKIP) return;
    for (const command of group.commands) {
      this.queue.send(command).then(
        (frame) => {
          if (frame.direction === "response") this.onData(command, frame.payload);
        },
        () => {
          // Timeout / disconnect / an FC that doesn't implement the command.
          // The queue retries internally; a persistent miss just stays "—".
        },
      );
    }
  }
}

// ── Rolling snapshot ───────────────────────────────────────────

export type MspLinkState = "connecting" | "live" | "error" | "closed";

/** The decoded telemetry the Sensors view reads. Fields stay null until a frame
 *  for them decodes — the UI renders "—", never zeros-as-live. */
export interface MspTelemetrySnapshot {
  linkState: MspLinkState;
  error: string | null;
  /** Date.now() ms of the most recent decoded frame, or null. */
  lastFrameAt: number | null;
  attitude: MspAttitude | null;
  analog: MspAnalog | null;
  gps: MspRawGps | null;
  altitude: MspAltitude | null;
  rc: number[] | null;
  status: MspStatus | null;
  sensors: SensorFlag[] | null;
}

function emptySnapshot(): MspTelemetrySnapshot {
  return {
    linkState: "connecting",
    error: null,
    lastFrameAt: null,
    attitude: null,
    analog: null,
    gps: null,
    altitude: null,
    rc: null,
    status: null,
    sensors: null,
  };
}

const CONNECT_TIMEOUT_MS = 4000;

/**
 * One live MSP telemetry session over the agent proxy. Owns a single WS; call
 * `disconnect()` when done. Read the rolling state via `snapshot()`.
 *
 * Not concurrency-safe with the settings client (`MspFcClient`) — both dial the
 * same serial link — but the dashboard only mounts one MSP surface at a time
 * (the Parameters and Sensors tabs are mutually exclusive).
 */
export class MspTelemetryClient {
  private transport = new WebSocketTransport();
  private parser = new MspParser();
  private queue: MspSerialQueue | null = null;
  private poller: MspTelemetryPoller | null = null;
  private snap: MspTelemetrySnapshot = emptySnapshot();

  constructor(private readonly firmware: MspVariant) {}

  private onData = (data: Uint8Array): void => {
    this.parser.feed(data);
  };

  private onFrame = (command: number, payload: Uint8Array): void => {
    switch (command) {
      case MSP_CMD.MSP_ATTITUDE: {
        const a = decodeAttitude(payload);
        if (a) this.snap.attitude = a;
        break;
      }
      case MSP_CMD.MSP_ANALOG: {
        const a = decodeAnalog(payload);
        if (a) this.snap.analog = a;
        break;
      }
      case MSP_CMD.MSP_RAW_GPS: {
        const g = decodeRawGps(payload);
        if (g) this.snap.gps = g;
        break;
      }
      case MSP_CMD.MSP_ALTITUDE: {
        const al = decodeAltitude(payload);
        if (al) this.snap.altitude = al;
        break;
      }
      case MSP_CMD.MSP_RC: {
        const rc = decodeRc(payload);
        if (rc) this.snap.rc = rc;
        break;
      }
      case MSP_CMD.MSP_STATUS_EX: {
        const s = decodeStatusEx(payload);
        if (s) {
          this.snap.status = s;
          this.snap.sensors = decodeSensorFlags(s.sensors, this.firmware);
        }
        break;
      }
      case MSP_CMD.MSP2_INAV_STATUS: {
        const s = decodeInavStatus(payload);
        if (s) {
          this.snap.status = s;
          this.snap.sensors = decodeSensorFlags(s.sensors, this.firmware);
        }
        break;
      }
      default:
        return;
    }
    this.snap.lastFrameAt = Date.now();
    this.snap.linkState = "live";
  };

  /** Open the WS proxy (ticket-authenticated when paired) and start polling. */
  async connect(signal?: AbortSignal): Promise<void> {
    const ticket = await mintMavlinkWsTicket(signal);
    const url = mavlinkWsUrl();
    try {
      await Promise.race([
        this.transport.connect(url, ticket ? [WS_TICKET_PROTOCOL, ticket] : undefined),
        new Promise<never>((_, reject) =>
          setTimeout(() => reject(new Error("WebSocket connect timeout")), CONNECT_TIMEOUT_MS),
        ),
      ]);
    } catch (err) {
      this.snap.linkState = "error";
      this.snap.error = err instanceof Error ? err.message : String(err);
      throw err;
    }
    this.transport.on("data", this.onData);
    this.transport.on("close", this.handleClose);
    // The queue subscribes to the parser and resolves each polled request to its
    // matching response; decode flows through the poller's onData callback, so
    // no separate parser subscription is needed (that would double-decode and
    // also react to unsolicited frames).
    this.queue = new MspSerialQueue((bytes) => this.transport.send(bytes), this.parser);
    this.poller = new MspTelemetryPoller(
      this.queue,
      pollGroups(this.firmware),
      this.onFrame,
    );
    this.poller.start();
  }

  private handleClose = (): void => {
    if (this.snap.linkState !== "error") this.snap.linkState = "closed";
  };

  /** A shallow copy of the current telemetry. */
  snapshot(): MspTelemetrySnapshot {
    return { ...this.snap };
  }

  /** Tear down the poller, queue, and WS. Idempotent. */
  async disconnect(): Promise<void> {
    this.poller?.stop();
    this.poller = null;
    this.transport.off("data", this.onData);
    this.transport.off("close", this.handleClose);
    this.queue?.destroy();
    this.queue = null;
    this.parser.reset();
    await this.transport.disconnect();
  }
}
