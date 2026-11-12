import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("pairing page", () => {
  it("shows the RF link card on a drone and a ground station", () => {
    expect(read("routes/pairing-route.tsx")).toMatch(
      /\(profile === "drone" \|\| profile === "ground_station"\) && <RfLinkCard \/>/,
    );
  });

  it("unbinds the radio behind a confirm with the flight warning, forcing only after the armed question", () => {
    const card = read("components/pairing/rf-link-card.tsx");
    expect(card).toMatch(/onClick=\{\(\) => setConfirm\(true\)\}/);
    expect(card).toMatch(/pairing\.confirm\.flightWarning/);
    expect(card).toMatch(/withArmedRetry\(/);
    expect(card).toMatch(/force \? "\/api\/wfb\/pair\/unpair\?force=1"/);
  });

  it("never prints a raw error message", () => {
    expect(read("routes/pairing-route.tsx")).not.toMatch(/error\.message/);
  });
});
