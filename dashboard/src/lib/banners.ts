// Page-level banners shown above every route. Pure so the rules are testable.

import i18n from "@/i18n";

import type { Profile, Severity } from "./types";

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
}

export function buildBanners({
  agentUnreachable,
  profile,
  fcConnected,
  fcReachable,
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

  return banners;
}
