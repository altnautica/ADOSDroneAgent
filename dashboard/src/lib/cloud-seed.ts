// Seed values for the Cloud settings self-hosted form. Setup status
// `cloud_choice` carries only mode + backend_url, so the broker and port come
// from the config's `server.self_hosted` block (the model default port is
// 8883). Seeding from config means re-applying never blanks the broker or
// resets the port.

import type { AgentConfig } from "@/hooks/use-config";

export interface SelfHostedSeed {
  url: string;
  broker: string;
  port: number;
}

export const DEFAULT_MQTT_PORT = 8883;

export function selfHostedSeed(config: AgentConfig | undefined): SelfHostedSeed {
  const sh = config?.server?.self_hosted;
  return {
    url: sh?.url ?? "",
    broker: sh?.mqtt_broker ?? "",
    port: sh?.mqtt_port ?? DEFAULT_MQTT_PORT,
  };
}
