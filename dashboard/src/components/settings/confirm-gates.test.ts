import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

// Link-dropping and corrections-stopping writes fire only from a confirm.
const read = (f: string) => readFileSync(path.resolve(__dirname, "../..", f), "utf8");

describe("confirm before a write that cuts a link", () => {
  it("Wi-Fi Disconnect opens a confirm instead of leaving at once", () => {
    const src = read("components/wifi/wifi-panel.tsx");
    expect(src).not.toMatch(/onClick=\{\(\) => leaveMutation\.mutate\(\)\}/);
    expect(src).toMatch(/onConfirm=\{\(\) => leaveMutation\.mutateAsync\(\)/);
  });

  it("hotspot off asks first", () => {
    const src = read("routes/settings/network-settings.tsx");
    expect(src).toMatch(/if \(!next\) \{\s*setPendingDisable\(true\);/);
  });

  it("Make primary confirms and is refused while armed or unknown", () => {
    const src = read("routes/io-route.tsx");
    expect(src).not.toMatch(/onClick=\{\(\) => void makePrimary/);
    expect(src).toMatch(/if \(block\) \{\s*toast\.err\(blockText\);\s*return;/);
    expect(src).toContain("setPrimaryPad(deviceId,");
  });

  it("GNSS base survey and off confirm with the RTCM targets", () => {
    const src = read("components/panels/gnss-panel.tsx");
    expect(src).not.toMatch(/onClick=\{\(\) => setBase\(/);
    expect(src).toMatch(/t\("gnss\.rtcmLoss", \{ count: /);
  });

  it("capture Stop confirms", () => {
    const src = read("components/panels/capture-panel.tsx");
    expect(src).not.toMatch(/onClick=\{onStop\}/);
    expect(src).toMatch(/onConfirm=\{onStop\}/);
  });

  it("Unpair carries a flight warning and retries with the armed override only on a yes", () => {
    const src = read("routes/pairing-route.tsx");
    expect(src).toMatch(/pairing\.confirm\.flightWarning/);
    expect(src).toMatch(/withArmedRetry\(\s*\(force\) => unpair\.mutateAsync\(force\)/);
    expect(read("hooks/use-pairing.ts")).toMatch(/force \? "\/api\/pairing\/unpair\?force=1"/);
  });

  it("the armed-override question is a locale string", () => {
    for (const f of ["routes/settings/profile-settings.tsx", "routes/pairing-route.tsx"]) {
      expect(read(f)).toMatch(/description=\{t\("settings\.shared\.armedOverride"\)\}/);
    }
  });

  it("a failing confirm action never escapes as an unhandled rejection", () => {
    expect(read("components/settings/confirm-dialog.tsx")).toMatch(/await onConfirm\(\);[\s\S]*\} catch \{/);
  });
});
