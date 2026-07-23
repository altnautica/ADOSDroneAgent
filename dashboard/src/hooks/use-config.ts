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
  ground_station?: {
    role?: string;
  };
  network?: {
    wifi_client?: { ssid?: string };
    hotspot?: { enabled?: boolean };
    cellular?: { enabled?: boolean; apn?: string };
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
