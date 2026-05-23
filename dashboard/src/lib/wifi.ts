// Helpers for the Wi-Fi panel: REST wrappers + presentation utilities.

import { apiFetch } from "./api";
import type {
  WifiForgetResult,
  WifiJoinResult,
  WifiLeaveResult,
  WifiNetwork,
  WifiSavedConnection,
  WifiStatus,
} from "./types";

export function getWifiStatus() {
  return apiFetch<WifiStatus>("/api/v1/network/client/status");
}

export function scanWifi() {
  return apiFetch<{ networks: WifiNetwork[] }>(
    "/api/v1/network/client/scan",
  );
}

export function getSavedWifi() {
  return apiFetch<{ connections: WifiSavedConnection[] }>(
    "/api/v1/network/client/configured",
  );
}

export function joinWifi(
  ssid: string,
  passphrase: string | null,
  force = false,
) {
  return apiFetch<WifiJoinResult>("/api/v1/network/client/join", {
    method: "PUT",
    body: { ssid, passphrase, force },
  });
}

export function leaveWifi() {
  return apiFetch<WifiLeaveResult>("/api/v1/network/client", {
    method: "DELETE",
  });
}

export function forgetWifi(name: string) {
  return apiFetch<WifiForgetResult>(
    `/api/v1/network/client/configured/${encodeURIComponent(name)}`,
    { method: "DELETE" },
  );
}

export function setWifiAutoconnect(name: string, enabled: boolean) {
  return apiFetch<{
    autoconnect: boolean;
    name: string;
    error: string | null;
  }>(
    `/api/v1/network/client/configured/${encodeURIComponent(name)}/autoconnect`,
    { method: "PUT", body: { enabled } },
  );
}

// nmcli reports SIGNAL on a 0-100 percent scale (derived from dBm).
// Map to four discrete bars so the row UI matches the macOS / iOS
// convention without needing to compute a precise dBm number.
export function signalToBars(signal: number | null | undefined): 0 | 1 | 2 | 3 | 4 {
  if (signal == null || !isFinite(signal)) return 0;
  if (signal < 25) return 1;
  if (signal < 50) return 2;
  if (signal < 75) return 3;
  return 4;
}

// Normalize the nmcli SECURITY column. nmcli writes "--" for an open
// network; collapsing that here keeps the row UI logic single-branch.
export function securityLabel(security: string | null | undefined): string {
  if (!security) return "open";
  const trimmed = security.trim();
  if (!trimmed || trimmed === "--") return "open";
  return trimmed;
}

export function isSecured(security: string | null | undefined): boolean {
  return securityLabel(security) !== "open";
}
