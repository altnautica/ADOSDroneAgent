/**
 * Small shared shapes for the dashboard's MSP settings clients.
 *
 * @module lib/msp/types
 * @license GPL-3.0-only
 */

/** A named CLI setting and its current raw text value. */
export interface CliSetting {
  name: string;
  value: string;
}

/** A staged CLI setting change (name + new raw text value). */
export interface CliSettingChange {
  name: string;
  value: string;
}

/** Result of a settings write. */
export interface CommandResult {
  success: boolean;
  resultCode: number;
  message: string;
}

/**
 * Text-CLI settings surface (the Betaflight `#` CLI: `get` / `set` / `dump` /
 * `save noreboot`). Reads and writes the ~810 named settings that have no
 * MSP2_COMMON_SETTING introspection.
 */
export interface CliSettingsCapability {
  enumerate(): Promise<CliSetting[]>;
  getSetting(name: string): Promise<string | undefined>;
  applySettings(changes: CliSettingChange[], opts?: { persist?: boolean }): Promise<CommandResult>;
}
