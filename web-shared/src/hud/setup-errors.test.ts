import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import strings from "../cockpit-spec/setup-strings.json";

const S: Record<string, string> = strings.strings;
const SETUP_RS = path.resolve(__dirname, "../../../crates/ados-pilot/src/setup/mod.rs");

/** Every literal error code the pilot's setup handler answers with. */
function pilotSetupErrors(): string[] {
  const src = readFileSync(SETUP_RS, "utf8");
  return [...new Set([...src.matchAll(/reply_err\("([a-z_]+)"\)/g)].map((m) => m[1]))].sort();
}

describe("setup error words", () => {
  it("finds the pilot's setup error codes", () => {
    expect(pilotSetupErrors()).toContain("not_running");
  });

  it("word every setup error code the pilot can answer with", () => {
    const missing = pilotSetupErrors().filter((code) => !S[`setup.err.${code}`]);
    expect(missing).toEqual([]);
  });

  it("say the onboard pilot is not running for `not_running`", () => {
    expect(S["setup.err.not_running"]).toBe("The onboard pilot is not running");
  });
});
