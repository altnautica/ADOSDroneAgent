import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";

// Loose shape — the agent returns a Pydantic dump with secrets redacted.
// We only consume the agent slice for log level and a couple of advanced
// fields. Everything else is opaque pass-through and ignored.
export interface AgentConfig {
  agent?: {
    profile?: string;
    // Injected by GET /api/config from the /etc/ados/board_override file the
    // HAL detector reads; empty string = auto-detect.
    board_override?: string;
  };
  logging?: {
    level?: string;
  };
  mavlink?: {
    source?: "auto" | "serial" | "udp" | "tcp";
    serial_port?: string;
    baud_rate?: number;
    system_id?: number;
    component_id?: number;
    ws_proxy_enforce_auth?: boolean;
    endpoints?: Array<{
      type?: string;
      host?: string;
      port?: number;
      enabled?: boolean;
    }>;
  };
  ground_station?: {
    role?: string;
  };
  network?: {
    wifi_client?: { ssid?: string };
    hotspot?: { enabled?: boolean };
    cellular?: { enabled?: boolean; apn?: string };
    mac_pin?: { enabled?: boolean; apply_live_allowed?: boolean };
    wifi_selfheal?: {
      enabled?: boolean;
      fail_threshold?: number;
      cooldown_s?: number;
    };
  };
  video?: {
    usb_recovery?: {
      enabled?: boolean;
      allow_ppps?: boolean;
      cold_boot_enum_aid?: boolean;
    };
  };
  server?: {
    mode?: string;
    self_hosted?: {
      url?: string;
      mqtt_broker?: string;
      mqtt_port?: number;
      api_key?: string;
    };
  };
  // GET /api/config redacts secrets: api_key comes back as the "***" sentinel
  // when set, "" when unset. The dashboard only reads the set/unset state.
  security?: {
    hmac_enabled?: boolean;
    setup_token_required?: boolean;
    api?: {
      api_key?: string;
      cors_enabled?: boolean;
    };
  };
  vision?: {
    enabled?: boolean;
    backend?: string;
    confidence_threshold?: number;
    models_dir?: string;
    models_cache_max_mb?: number;
    auto_download?: boolean;
  };
  // Two-tier perception execution: the drone-side offload target + the
  // workstation-side serving toggle. Read on the Offload settings page.
  perception?: {
    offload?: {
      enabled?: string;
      compute_node_addr?: string;
    };
    serving?: {
      enabled?: string;
      detector_model?: string;
    };
  };
  [key: string]: unknown;
}

export function useConfig() {
  return useQuery<AgentConfig>({
    queryKey: ["config"],
    queryFn: () => apiFetch<AgentConfig>("/api/config"),
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
}
