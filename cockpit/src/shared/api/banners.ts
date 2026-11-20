// Page-level banners shown above every route. Pure so the rules are testable.

import i18n from "../i18n";

import type { Severity } from "../format";
import { repairingMisKeyed, type PairRearm } from "../hud/status-pill";
import { specText } from "../hud/spec-text";
import type { Profile } from "./types";

export interface Banner {
  id: string;
  severity: Severity;
  message: string;
}

export interface BannerInputs {
  /** The heartbeat (`/api/status`) poll failed. */
  agentUnreachable: boolean;
  /** Resolved node profile from the pairing-info probe. */
  profile: Profile;
  /** `snapshot.fc.connected`, or null when no snapshot was read. */
  fcConnected: boolean | null;
  /** Heartbeat `fcReachable` (covers MSP flight controllers too). */
  fcReachable: boolean | null | undefined;
  /** `GET /api/wfb` `pair_rearm` on a ground station, or null when unread. */
  pairRearm?: PairRearm | null;
}

export function buildBanners({
  agentUnreachable,
  profile,
  fcConnected,
  fcReachable,
  pairRearm,
}: BannerInputs): Banner[] {
  const banners: Banner[] = [];

  if (agentUnreachable) {
    banners.push({
      id: "agent-offline",
      severity: "err",
      message: i18n.t("layout.banners.agentOffline"),
    });
  }

  // An MSP flight controller never sends a MAVLink heartbeat, so
  // `fc.connected` stays false for it; `fcReachable` covers both.
  if (profile === "drone" && fcConnected === false && fcReachable !== true) {
    banners.push({
      id: "fc-disconnected",
      severity: "warn",
      message: i18n.t("layout.banners.fcDisconnected"),
    });
  }

  // The cockpit spec's `gs_repairing` alert, worded once for every surface.
  if (profile === "ground_station" && repairingMisKeyed(pairRearm)) {
    banners.push({
      id: "gs-repairing",
      severity: "warn",
      message: specText("alert.gs_repairing"),
    });
  }

  return banners;
}
