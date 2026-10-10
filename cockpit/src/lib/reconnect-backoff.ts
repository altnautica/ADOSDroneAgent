// Reconnect backoff for the cockpit's WebSockets. A socket that the server
// accepts and then closes straight away (a route with nothing behind it, such
// as the detection stream on a node with no vision engine) must not reset the
// backoff on open, or it redials every second forever. The delay resets only
// after a session proved itself: it delivered a message, or it stayed open for
// `STABLE_SESSION_MS`.

export const STABLE_SESSION_MS = 10_000;

export class ReconnectBackoff {
  private delay: number | null = null;
  private openedAt: number | null = null;
  private gotMessage = false;

  constructor(
    private readonly minMs: number,
    private readonly maxMs: number,
    private readonly now: () => number = () => performance.now(),
  ) {}

  opened(): void {
    this.openedAt = this.now();
    this.gotMessage = false;
  }

  message(): void {
    this.gotMessage = true;
  }

  /** The delay before the next dial, after a close or a failed dial. */
  next(): number {
    const stable =
      this.gotMessage || (this.openedAt !== null && this.now() - this.openedAt >= STABLE_SESSION_MS);
    if (stable) this.delay = null;
    this.openedAt = null;
    this.gotMessage = false;
    this.delay = this.delay === null ? this.minMs : Math.min(this.maxMs, this.delay * 2);
    return this.delay;
  }
}
