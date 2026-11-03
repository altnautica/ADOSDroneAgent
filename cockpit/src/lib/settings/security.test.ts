import { describe, expect, it } from "vitest";

import {
  apiKeyView,
  ownKeyRisk,
  pinFormProblem,
  pinFormRequest,
  pinState,
  requestWsEnforce,
  settleWsEnforce,
} from "@/lib/settings/security";

describe("apiKeyView", () => {
  it("requires a key exactly when the node is paired", () => {
    expect(apiKeyView({ pairing: { paired: true }, pairingError: false, configuredApiKey: "" })).toEqual({
      state: "required",
      configuredKey: false,
    });
    expect(apiKeyView({ pairing: { paired: false }, pairingError: false, configuredApiKey: "***" })).toEqual({
      state: "notRequired",
      configuredKey: true,
    });
  });

  it("is loading until pairing answers, unknown when it fails", () => {
    expect(apiKeyView({ pairing: null, pairingError: false, configuredApiKey: "***" }).state).toBe("loading");
    expect(apiKeyView({ pairing: null, pairingError: true, configuredApiKey: "" }).state).toBe("unknown");
  });

  it("reads only the redaction sentinel as a configured key", () => {
    expect(apiKeyView({ pairing: { paired: true }, pairingError: false, configuredApiKey: "plain" }).configuredKey).toBe(false);
  });
});

describe("ownKeyRisk", () => {
  const paired = { state: "required" as const, configuredKey: true };

  it("knows the panel holds the operator key when it stored it from a rotation", () => {
    expect(ownKeyRisk({ storedKey: "ados_new", rotatedKey: "ados_new", view: paired })).toBe("operator");
  });

  it("treats any other stored key on a paired node with an operator key as possibly it", () => {
    expect(ownKeyRisk({ storedKey: "ados_held", rotatedKey: null, view: paired })).toBe("possible");
    expect(ownKeyRisk({ storedKey: "ados_held", rotatedKey: "ados_other", view: paired })).toBe("possible");
    expect(ownKeyRisk({ storedKey: "ados_held", rotatedKey: null, view: { ...paired, state: "unknown" } })).toBe(
      "possible",
    );
  });

  it("is none with no stored key, no operator key, or an unpaired node", () => {
    expect(ownKeyRisk({ storedKey: null, rotatedKey: null, view: paired })).toBe("none");
    expect(ownKeyRisk({ storedKey: "k", rotatedKey: "k", view: { ...paired, configuredKey: false } })).toBe("none");
    expect(ownKeyRisk({ storedKey: "k", rotatedKey: "k", view: { ...paired, state: "notRequired" } })).toBe("none");
  });
});

describe("pinState", () => {
  it("never reads a failed status as not set", () => {
    expect(pinState({ pin_set: false, locked: false, locked_until: null }, true)).toBe("unknown");
  });

  it("maps a real status", () => {
    expect(pinState(null, false)).toBe("loading");
    expect(pinState({ pin_set: true, locked: true, locked_until: 5 }, false)).toBe("locked");
    expect(pinState({ pin_set: true, locked: false, locked_until: null }, false)).toBe("set");
    expect(pinState({ pin_set: false, locked: false, locked_until: null }, false)).toBe("notSet");
  });
});

describe("WS auth enforcement gate", () => {
  it("does not write on enable until the slide confirms", () => {
    const asked = requestWsEnforce(true);
    expect(asked.write).toBeNull();
    expect(asked.gate.open).toBe(true);

    const dismissed = settleWsEnforce(asked.gate, false);
    expect(dismissed.write).toBeNull();
    expect(dismissed.gate.open).toBe(false);

    const slid = settleWsEnforce(asked.gate, true);
    expect(slid.write).toBe(true);
    expect(slid.gate.open).toBe(false);
  });

  it("a confirm with no pending request writes nothing", () => {
    expect(settleWsEnforce(requestWsEnforce(false).gate, true).write).toBeNull();
  });

  it("writes a disable at once, with no slide", () => {
    expect(requestWsEnforce(false)).toEqual({ write: false, gate: { open: false } });
  });
});

describe("PIN form gate", () => {
  const change = { pinSet: true, current: "1234", pin: "567890", confirm: "567890" };

  it("sends a change with the current PIN, a first set without", () => {
    expect(pinFormRequest(change)).toEqual({ pin: "567890", currentPin: "1234" });
    expect(pinFormRequest({ pinSet: false, current: "", pin: "4321", confirm: "4321" })).toEqual({ pin: "4321" });
  });

  it("holds the guarded change while the form is not valid", () => {
    expect(pinFormProblem({ ...change, current: "12" })).toBe("current");
    expect(pinFormProblem({ ...change, pin: "12a4", confirm: "12a4" })).toBe("pin");
    expect(pinFormProblem({ ...change, confirm: "567891" })).toBe("mismatch");
    expect(pinFormProblem({ ...change, pin: "1234", confirm: "1234" })).toBe("same");
    expect(pinFormRequest({ ...change, confirm: "" })).toBeNull();
  });

  it("does not ask for a current PIN when none is set", () => {
    expect(pinFormProblem({ pinSet: false, current: "", pin: "123456", confirm: "123456" })).toBeNull();
  });
});
