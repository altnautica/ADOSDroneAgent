/**
 * Betaflight CLI session — a raw-ASCII command channel over the serial link.
 *
 * Betaflight exposes its ~810 named settings only through the CLI (`get` /
 * `set` / `dump`), which is plain, un-framed ASCII entered by sending `#`. The
 * MSP parser only surfaces STX/ETX-framed CLI blocks, so it silently drops
 * Betaflight's ASCII CLI output (and a stray `$` would corrupt its state).
 * This session therefore taps the raw inbound bytes directly: while a session
 * is active the adapter routes bytes here instead of into the MSP parser, and
 * pauses MSP polling (the FC speaks only CLI until we exit).
 *
 * Leaving the CLI uses `exit noreboot` (or `save noreboot` to persist to
 * EEPROM) so a settings read/write never reboots the flight controller.
 *
 * @module protocol/msp/bf-cli
 */

const enc = (s: string): Uint8Array => new TextEncoder().encode(s);
const decoder = new TextDecoder();

/** Adapter-provided I/O for a CLI session. */
export interface BfCliIo {
  /** Write raw bytes to the serial link. */
  send(bytes: Uint8Array): void;
  /**
   * Flip the adapter's inbound-byte routing to this session and pause MSP
   * polling (active=true), or restore MSP parsing and polling (active=false).
   */
  setActive(active: boolean): void;
}

const IDLE_MS = 400;
const PROMPT_GRACE_MS = 80;
const CMD_TIMEOUT_MS = 4000;

/**
 * True when the buffer's last line is the FC's interactive `#` prompt (just
 * `#` + optional space, the FC waiting for input) rather than a `# comment`
 * line inside a `dump` (which has text after the `#`).
 */
function endsWithPrompt(buf: string): boolean {
  const lastLine = buf.slice(buf.lastIndexOf("\n") + 1);
  return /^\s*#\s*$/.test(lastLine);
}

/**
 * A single connected Betaflight CLI session. Not concurrency-safe: enter →
 * run…* → exit is a serial sequence owned by one caller at a time.
 */
export class BfCliSession {
  private buffer = "";
  private notify: (() => void) | null = null;
  private active = false;
  private interactiveCb: ((text: string) => void) | null = null;

  constructor(private readonly io: BfCliIo) {}

  get isActive(): boolean {
    return this.active;
  }

  /** Feed raw inbound bytes (called by the adapter while a session is active). */
  feed(data: Uint8Array): void {
    const text = decoder.decode(data, { stream: true });
    if (this.interactiveCb) {
      this.interactiveCb(text); // interactive terminal: stream, don't buffer for collect
      return;
    }
    this.buffer += text;
    this.notify?.();
  }

  // ── Interactive terminal mode (for the CLI panel) ───────────

  /** Open an interactive session: enter the CLI and stream all inbound text to `cb`. */
  attachInteractive(cb: (text: string) => void): void {
    this.interactiveCb = cb;
    if (!this.active) {
      this.active = true;
      this.io.setActive(true);
      this.io.send(enc("#\r\n"));
    }
  }

  /** Send a raw command line in interactive mode. */
  sendInteractive(line: string): void {
    if (!this.active) {
      this.active = true;
      this.io.setActive(true);
    }
    this.io.send(enc(`${line}\r\n`));
  }

  /** Close the interactive session, leaving the CLI without a reboot. */
  detachInteractive(): void {
    this.interactiveCb = null;
    if (this.active) {
      this.io.send(enc("exit noreboot\r\n"));
      this.active = false;
      this.io.setActive(false);
    }
  }

  /** Enter the CLI (`#`). Returns the banner text. Idempotent. */
  async enter(): Promise<string> {
    if (this.active) return "";
    this.active = true;
    this.io.setActive(true);
    this.buffer = "";
    this.io.send(enc("#\r\n"));
    return this.collect(CMD_TIMEOUT_MS);
  }

  /** Send one CLI command and collect its output up to the next prompt. */
  async run(cmd: string, timeoutMs = CMD_TIMEOUT_MS): Promise<string> {
    if (!this.active) throw new Error("BF CLI session is not active");
    this.buffer = "";
    this.io.send(enc(`${cmd}\r\n`));
    return this.collect(timeoutMs);
  }

  /** Leave the CLI. `persist` writes EEPROM first (`save noreboot`); neither reboots. */
  async exit(persist = false): Promise<void> {
    if (!this.active) return;
    try {
      this.buffer = "";
      this.io.send(enc(persist ? "save noreboot\r\n" : "exit noreboot\r\n"));
      await this.collect(CMD_TIMEOUT_MS).catch(() => undefined);
    } finally {
      this.active = false;
      this.io.setActive(false);
    }
  }

  /**
   * Resolve when the CLI prompt returns, the stream goes idle, or a timeout
   * elapses. A prompt schedules a short grace rather than resolving inline, so a
   * TCP chunk that happens to end at a mid-`dump` `#` cannot cut the read short:
   * if more bytes follow, the timer re-arms; if the stream is truly done, the
   * grace elapses.
   */
  private collect(timeoutMs: number): Promise<string> {
    return new Promise((resolve) => {
      let idle: ReturnType<typeof setTimeout> | undefined;
      const finish = (): void => {
        if (idle) clearTimeout(idle);
        clearTimeout(hard);
        this.notify = null;
        resolve(this.buffer);
      };
      const onData = (): void => {
        if (idle) clearTimeout(idle);
        idle = setTimeout(finish, endsWithPrompt(this.buffer) ? PROMPT_GRACE_MS : IDLE_MS);
      };
      const hard = setTimeout(finish, timeoutMs);
      this.notify = onData;
      onData(); // arm the timer / catch an already-complete buffer
    });
  }
}
