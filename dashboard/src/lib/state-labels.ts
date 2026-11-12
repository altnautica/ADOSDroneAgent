// Words for the agent's state codes. The panels show these, never the wire
// value: an unrecognised code reads as "unrecognised", not as the raw string.

import type { ParseKeys } from "i18next";

import type { BadgeVariant } from "@/components/ui/badge";
import i18n from "@/shared/i18n";

function pick(keys: Record<string, ParseKeys>, code: string | null | undefined): string {
  const key = code ? keys[code.toLowerCase()] : undefined;
  return i18n.t(key ?? "panels.state.unrecognised");
}

const SERVICE: Record<string, ParseKeys> = {
  active: "panels.state.service.active",
  inactive: "panels.state.service.inactive",
  failed: "panels.state.service.failed",
  activating: "panels.state.service.activating",
  deactivating: "panels.state.service.deactivating",
  reloading: "panels.state.service.reloading",
};

/** A systemd unit's active state. */
export function serviceStateLabel(state: string | null | undefined): string {
  return pick(SERVICE, state);
}

/** The tone of a systemd state: a unit on its way up or down is a warning,
 *  never an error. */
export function serviceStateTone(state: string | null | undefined): "ok" | "warn" | "err" | "idle" {
  switch (state?.toLowerCase()) {
    case "active":
      return "ok";
    case "failed":
      return "err";
    case "activating":
    case "deactivating":
    case "reloading":
      return "warn";
    default:
      return "idle";
  }
}

const WFB: Record<string, ParseKeys> = {
  active: "panels.state.wfb.active",
  ready: "panels.state.wfb.ready",
  connecting: "panels.state.wfb.connecting",
  stale: "panels.state.wfb.stale",
  error: "panels.state.wfb.error",
  disabled: "panels.state.wfb.disabled",
};

/** A WFB link state. */
export function wfbStateLabel(state: string | null | undefined): string {
  return pick(WFB, state);
}

const GNSS_FIX: Record<string, ParseKeys> = {
  none: "panels.state.gnssFix.none",
  "2d": "panels.state.gnssFix.fix2d",
  "3d": "panels.state.gnssFix.fix3d",
  dgps: "panels.state.gnssFix.dgps",
  rtk_float: "panels.state.gnssFix.rtkFloat",
  rtk_fixed: "panels.state.gnssFix.rtkFixed",
};

/** A receiver's fix quality. */
export function gnssFixLabel(fix: string | null | undefined): string {
  return pick(GNSS_FIX, fix);
}

const GNSS_BASE: Record<string, ParseKeys> = { off: "panels.state.gnssBase.off", surveying: "panels.state.gnssBase.surveying", fixed: "panels.state.gnssBase.fixed", failed: "panels.state.gnssBase.failed" };

/** The base machine state. */
export function gnssBaseLabel(state: string | null | undefined): string {
  return pick(GNSS_BASE, state);
}

const VIDEO: Record<string, ParseKeys> = { running: "panels.state.video.running", ready: "panels.state.video.ready", no_camera: "panels.state.video.noCamera", stopped: "panels.state.video.stopped" };

/** The video pipeline verdict. */
export function videoStateLabel(state: string | null | undefined): string {
  return pick(VIDEO, state);
}

const STORAGE: Record<string, ParseKeys> = { ok: "panels.state.storage.ok", near_full: "panels.state.storage.nearFull", full: "panels.state.storage.full" };

/** The capture budget state. */
export function storageStateLabel(state: string | null | undefined): string {
  return pick(STORAGE, state);
}

export function storageStateVariant(state: string | null | undefined): BadgeVariant {
  return state === "ok" ? "success" : state === "full" ? "error" : "warning";
}

const LORA_SOURCE: Record<string, ParseKeys> = { "lora.region": "panels.state.loraSource.loraRegion", "network.regulatory.region": "panels.state.loraSource.country" };

/** Where the LoRa region came from (the config key the agent names). */
export function loraSourceLabel(source: string | null | undefined): string {
  return pick(LORA_SOURCE, source);
}
