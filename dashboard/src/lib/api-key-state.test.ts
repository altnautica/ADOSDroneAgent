import { describe, expect, it } from "vitest";

import { apiKeyView, REDACTED } from "./api-key-state";

const paired = { paired: true };
const unpaired = { paired: false };

describe("apiKeyView", () => {
  it("reads a paired node as key-required even with no configured key", () => {
    expect(apiKeyView({ pairing: paired, pairingError: false, configuredApiKey: "" })).toEqual({
      state: "required",
      configuredKey: false,
    });
  });

  it("reads a paired node with a configured second key as key-required", () => {
    expect(apiKeyView({ pairing: paired, pairingError: false, configuredApiKey: REDACTED })).toEqual({
      state: "required",
      configuredKey: true,
    });
  });

  it("does not claim a configured key is enforced on an unpaired node", () => {
    expect(apiKeyView({ pairing: unpaired, pairingError: false, configuredApiKey: REDACTED })).toEqual({
      state: "notRequired",
      configuredKey: true,
    });
  });

  it("reads an unpaired node with no configured key as not required", () => {
    expect(apiKeyView({ pairing: unpaired, pairingError: false, configuredApiKey: undefined })).toEqual({
      state: "notRequired",
      configuredKey: false,
    });
  });

  it("waits for the pairing state rather than guessing", () => {
    expect(apiKeyView({ pairing: undefined, pairingError: false, configuredApiKey: "" }).state).toBe("loading");
  });

  it("reports unknown when the pairing state cannot be read", () => {
    expect(apiKeyView({ pairing: undefined, pairingError: true, configuredApiKey: "" }).state).toBe("unknown");
  });
});
