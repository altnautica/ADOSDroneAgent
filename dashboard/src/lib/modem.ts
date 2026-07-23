// REST wrappers + response types for the ground-station cellular modem: the
// mmcli-backed presence probe, the config + usage view, and the config write.
// Profile-gated on the agent (they 404 on a drone / workstation), so the
// Cellular page only calls them once it knows the node is a ground station.

import { apiFetch } from "./api";

// GET /api/v1/ground-station/modem-status — mmcli-backed presence + facts. A
// missing modem / no ModemManager reports present:false with a reason; a
// detected modem carries the reported operator / technology / signal.
export interface ModemPresence {
  present: boolean;
  reason?: string;
  operator?: string;
  tech?: string;
  band?: string;
  rssi_pct?: number | null;
  rssi_dbm?: number | null;
  rsrp_dbm?: number | null;
  rsrq_db?: number | null;
  sinr_db?: number | null;
  ip?: string;
}

// GET / PUT /api/v1/ground-station/network/modem — the config + usage view. The
// PUT response IS the read-back (the view over the freshly-persisted config).
// Sentinels the manager ships when no modem is present: signal_quality = -1,
// technology = "unknown", operator = "" — treated as unknown, never as facts.
export interface ModemView {
  enabled?: boolean;
  connected?: boolean;
  iface?: string | null;
  ip?: string | null;
  signal_quality?: number | null;
  technology?: string | null;
  apn?: string | null;
  operator?: string | null;
  data_used_mb?: number;
  cap_mb?: number;
  percent?: number;
  state?: string | null;
}

export interface ModemUpdate {
  enabled?: boolean;
  apn?: string;
  cap_gb?: number;
}

export function getModemStatus() {
  return apiFetch<ModemPresence>("/api/v1/ground-station/modem-status");
}

export function getModemView() {
  return apiFetch<ModemView>("/api/v1/ground-station/network/modem");
}

// Persist a modem-config change. The response is the modem view over the
// freshly-persisted config, so rendering it IS the read-back.
export function setModem(update: ModemUpdate) {
  return apiFetch<ModemView>("/api/v1/ground-station/network/modem", {
    method: "PUT",
    body: update,
  });
}
