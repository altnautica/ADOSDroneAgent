/**
 * MSP serial request queue.
 *
 * MSP is a request/response protocol with one outstanding request at a time.
 * This queue serializes concurrent send() calls, matching responses to
 * requests by command code, with timeout and retry.
 *
 * @module protocol/msp/msp-serial-queue
 */

import { encodeMsp } from './codec';
import type { MspParser, ParsedMspFrame } from './parser';

// ── Types ──────────────────────────────────────────────────

interface PendingRequest {
  command: number;
  payload: Uint8Array | undefined;
  resolve: (frame: ParsedMspFrame) => void;
  reject: (error: Error) => void;
  retries: number;
}

// ── Queue Class ────────────────────────────────────────────

export class MspSerialQueue {
  private queue: PendingRequest[] = [];
  private active: PendingRequest | null = null;
  private timeoutId: ReturnType<typeof setTimeout> | null = null;
  private unsubscribe: (() => void) | null = null;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;

  constructor(
    private sendFn: (data: Uint8Array) => void,
    parser: MspParser,
    timeout = 1000,
    maxRetries = 2,
  ) {
    this.timeoutMs = timeout;
    this.maxRetries = maxRetries;

    // Subscribe to parsed frames
    this.unsubscribe = parser.onFrame((frame) => this.handleFrame(frame));
  }

  /**
   * Send an MSP command and wait for matching response.
   * If another request is in flight, this queues behind it.
   */
  send(command: number, payload?: Uint8Array): Promise<ParsedMspFrame> {
    return new Promise<ParsedMspFrame>((resolve, reject) => {
      this.queue.push({ command, payload, resolve, reject, retries: 0 });
      this.processNext();
    });
  }

  /**
   * Send without waiting for response (fire-and-forget).
   * Used for high-frequency commands like MSP_SET_RAW_RC.
   * Does NOT go through the queue; sends immediately.
   */
  sendNoReply(command: number, payload?: Uint8Array): void {
    const encoded = encodeMsp(command, payload);
    this.sendFn(encoded);
  }

  /**
   * Flush all pending requests. Rejects them with "Disconnected".
   * Call on transport disconnect.
   */
  flush(): void {
    this.clearTimeout();

    if (this.active) {
      this.active.reject(new Error('Disconnected'));
      this.active = null;
    }

    for (const req of this.queue) {
      req.reject(new Error('Disconnected'));
    }
    this.queue.length = 0;
  }

  /** Number of pending requests (including the active one). */
  get pending(): number {
    return this.queue.length + (this.active ? 1 : 0);
  }

  /** Unsubscribe from parser and flush. Call on cleanup. */
  destroy(): void {
    this.flush();
    if (this.unsubscribe) {
      this.unsubscribe();
      this.unsubscribe = null;
    }
  }

  // ── Internal ─────────────────────────────────────────────

  private processNext(): void {
    if (this.active || this.queue.length === 0) return;

    this.active = this.queue.shift()!;
    this.sendActive();
  }

  private sendActive(): void {
    if (!this.active) return;

    const encoded = encodeMsp(this.active.command, this.active.payload);
    this.sendFn(encoded);
    this.startTimeout();
  }

  private handleFrame(frame: ParsedMspFrame): void {
    if (!this.active) return;

    // Match response command to active request
    if (frame.command === this.active.command) {
      this.clearTimeout();
      const req = this.active;
      this.active = null;
      // An MSP error frame ($X!/$M!) means the FC rejected the command — reject
      // the request so a failed setting write surfaces instead of reading as saved.
      if (frame.direction === 'error') {
        req.reject(new Error(`MSP error for command ${frame.command}`));
      } else {
        req.resolve(frame);
      }
      this.processNext();
    }
    // Non-matching frames are ignored (could be unsolicited telemetry)
  }

  private startTimeout(): void {
    this.clearTimeout();
    this.timeoutId = setTimeout(() => {
      if (!this.active) return;

      if (this.active.retries < this.maxRetries) {
        this.active.retries++;
        this.sendActive();
      } else {
        const req = this.active;
        this.active = null;
        req.reject(
          new Error(`MSP timeout: command ${req.command} after ${this.maxRetries + 1} attempts`),
        );
        this.processNext();
      }
    }, this.timeoutMs);
  }

  private clearTimeout(): void {
    if (this.timeoutId !== null) {
      clearTimeout(this.timeoutId);
      this.timeoutId = null;
    }
  }
}
