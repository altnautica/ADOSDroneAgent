/**
 * MSP settings client for the browser dashboard.
 *
 * Betaflight and iNav have no `/api/params` cache on the agent (the agent is a
 * byte-pipe for MSP), so the dashboard runs the MSP codec itself over the
 * transparent `ws://<host>:8765/` proxy and reads settings directly from the FC:
 *
 *   - iNav  — the name-indexed MSP2_COMMON_SETTING(_INFO) protocol carries every
 *             setting's type, range, enum labels, and current value inline.
 *   - Betaflight — the ~810 named settings live only in the `#` CLI; `dump`
 *             enumerates current values and the bundled catalog supplies the
 *             enum/range metadata separately.
 *
 * @module lib/msp/fc-settings
 * @license GPL-3.0-only
 */

import { categoryFromName } from "@/lib/params";

import { BfCliSession } from "./bf-cli";
import { BfCliSettings } from "./bf-cli-settings";
import { MspParser } from "./parser";
import { MspSerialQueue } from "./serial-queue";
import { SettingsClient, type SettingInfo } from "./settings-client";
import { WebSocketTransport, mavlinkWsUrl } from "./transport";
import { WS_TICKET_PROTOCOL, mintMavlinkWsTicket } from "./ws-ticket";
import type { CliSettingChange } from "./types";

/** MSP_EEPROM_WRITE — persist the RAM settings to EEPROM (no payload). */
const MSP_EEPROM_WRITE = 250;
const CONNECT_TIMEOUT_MS = 4000;

/** One selectable enum option: what the operator sees and what gets written. */
export interface MspOption {
  label: string;
  /** The value string a `set` writes for this option (iNav: code; BF: label). */
  send: string;
}

/**
 * A normalized FC setting, firmware-agnostic, for the dashboard table.
 * `value` is always the string a `set` would write (iNav: the code; Betaflight:
 * the CLI text / enum label); `displayValue` is the human label to show.
 */
export interface MspSetting {
  name: string;
  category: string;
  value: string;
  displayValue: string;
  /** Enum options for a dropdown, when the setting is a lookup. */
  options?: MspOption[];
  /** Inclusive numeric range for a ranged numeric setting. */
  range?: { min: number; max: number };
}

export type MspFirmware = "betaflight" | "inav";

/** Map one iNav SETTING_INFO into the normalized shape (enum labels + range
 *  come straight from the FC). */
function fromInav(info: SettingInfo): MspSetting {
  const labels = info.enumValues;
  const value = info.value;
  const valueStr = value !== undefined ? String(value) : "";
  if (labels && labels.length > 0) {
    const options = labels.map((label, i) => ({ label, send: String(info.min + i) }));
    const idx = value !== undefined ? value - info.min : -1;
    return {
      name: info.name,
      category: categoryFromName(info.name),
      value: valueStr,
      displayValue: idx >= 0 && idx < labels.length ? labels[idx] : valueStr,
      options,
    };
  }
  return {
    name: info.name,
    category: categoryFromName(info.name),
    value: valueStr,
    displayValue: valueStr,
    range: { min: info.min, max: info.max },
  };
}

/**
 * A live MSP settings session over the agent proxy. One instance owns one WS
 * connection; call `disconnect()` when done. Not concurrency-safe across
 * `enumerate` / `apply` — the caller serializes them.
 */
export class MspFcClient {
  private transport = new WebSocketTransport();
  private parser = new MspParser();
  private queue: MspSerialQueue | null = null;
  private bfCli: BfCliSession | null = null;
  private bfSettings: BfCliSettings | null = null;
  private cliActive = false;

  constructor(private readonly firmware: MspFirmware) {}

  /** Route inbound bytes to the CLI session while it's active, else the MSP parser. */
  private onData = (data: Uint8Array): void => {
    if (this.cliActive && this.bfCli) this.bfCli.feed(data);
    else this.parser.feed(data);
  };

  /** Open the WS proxy (ticket-authenticated when paired, bare otherwise). */
  async connect(signal?: AbortSignal): Promise<void> {
    const ticket = await mintMavlinkWsTicket(signal);
    const url = mavlinkWsUrl();
    await Promise.race([
      this.transport.connect(url, ticket ? [WS_TICKET_PROTOCOL, ticket] : undefined),
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error("WebSocket connect timeout")), CONNECT_TIMEOUT_MS),
      ),
    ]);
    this.transport.on("data", this.onData);
    this.queue = new MspSerialQueue(
      (bytes) => this.transport.send(bytes),
      this.parser,
    );
    if (this.firmware === "betaflight") {
      this.bfCli = new BfCliSession({
        send: (bytes) => this.transport.send(bytes),
        setActive: (active) => {
          this.cliActive = active;
        },
      });
      this.bfSettings = new BfCliSettings(this.bfCli);
    }
  }

  /** Enumerate every setting from the FC, normalized. */
  async enumerate(): Promise<MspSetting[]> {
    if (this.firmware === "inav") {
      if (!this.queue) throw new Error("not connected");
      const client = new SettingsClient(this.queue);
      const infos = await client.enumerateAllSettings();
      return infos.map(fromInav);
    }
    // Betaflight — dump current values; enum/range metadata is overlaid from the
    // bundled catalog by the caller (the FC's CLI gives values only).
    if (!this.bfSettings) throw new Error("not connected");
    const dumped = await this.bfSettings.enumerate();
    return dumped.map((s) => ({
      name: s.name,
      category: categoryFromName(s.name),
      value: s.value,
      displayValue: s.value,
    } satisfies MspSetting));
  }

  /**
   * Write staged changes to the FC, persisting to EEPROM. For iNav each change
   * is a typed `set` then one EEPROM write; for Betaflight the CLI applies all
   * changes and `save noreboot`s in one session.
   */
  async apply(changes: CliSettingChange[]): Promise<{ ok: boolean; message: string }> {
    if (changes.length === 0) return { ok: true, message: "No changes" };
    if (this.firmware === "inav") {
      if (!this.queue) throw new Error("not connected");
      const client = new SettingsClient(this.queue);
      const failed: string[] = [];
      for (const c of changes) {
        try {
          await client.set(c.name, /^-?\d+(\.\d+)?$/.test(c.value) ? Number(c.value) : c.value);
        } catch {
          failed.push(c.name);
        }
      }
      try {
        await this.queue.send(MSP_EEPROM_WRITE);
      } catch {
        // EEPROM write may not echo on every build; the RAM sets already landed.
      }
      return failed.length
        ? { ok: false, message: `Rejected: ${failed.join(", ")}` }
        : { ok: true, message: "Saved to EEPROM" };
    }
    if (!this.bfSettings) throw new Error("not connected");
    const res = await this.bfSettings.applySettings(changes, { persist: true });
    return { ok: res.success, message: res.message };
  }

  /** Tear down the WS and flush the queue. Idempotent. */
  async disconnect(): Promise<void> {
    this.transport.off("data", this.onData);
    this.queue?.destroy();
    this.queue = null;
    this.parser.reset();
    await this.transport.disconnect();
  }
}
