import { describe, expect, it } from "vitest";

import { DEFAULT_MQTT_PORT, selfHostedSeed } from "./cloud-seed";

describe("selfHostedSeed", () => {
  it("seeds url, broker and port from server.self_hosted", () => {
    expect(
      selfHostedSeed({
        server: {
          mode: "self_hosted",
          self_hosted: { url: "https://c.example.com", mqtt_broker: "mqtt.example.com", mqtt_port: 1883 },
        },
      }),
    ).toEqual({ url: "https://c.example.com", broker: "mqtt.example.com", port: 1883 });
  });

  it("falls back to empty fields and the model default port", () => {
    expect(selfHostedSeed(undefined)).toEqual({ url: "", broker: "", port: DEFAULT_MQTT_PORT });
  });
});
