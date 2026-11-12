import { readFileSync } from "node:fs";
import path from "node:path";

import type { TFunction } from "i18next";
import { describe, expect, it } from "vitest";

import {
  failureText,
  installName,
  installStatusLabel,
  installStatusVariant,
  isInstallUrl,
  isSha256OrEmpty,
} from "./plugin-install-view";

const t = ((key: string, opts?: Record<string, unknown>) =>
  opts ? `${key}:${JSON.stringify(opts)}` : key) as unknown as TFunction;
const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("installed extension rows", () => {
  it("labels every status through a locale key, never the raw enum", () => {
    for (const s of ["installed", "enabled", "running", "disabled", "failed", "incompatible"]) {
      expect(installStatusLabel(s, t)).toBe(`extensions.status.${s}`);
    }
    expect(installStatusLabel("weird", t)).toBe("extensions.status.unknown");
  });

  it("tones a failed start as an error and an incompatible one as a warning", () => {
    expect(installStatusVariant("failed")).toBe("error");
    expect(installStatusVariant("incompatible")).toBe("warning");
    expect(installStatusVariant("disabled")).toBe("neutral");
    expect(installStatusVariant("running")).toBe("success");
  });

  it("says why the host stopped it in one sentence", () => {
    expect(failureText(null, t)).toBeNull();
    expect(failureText("tampered: agent/py/x.py digest mismatch", t)).toBe("extensions.failure.tampered");
    expect(failureText("signer_revoked", t)).toBe("extensions.failure.signerRevoked");
    expect(failureText("exit 3", t)).toBe('extensions.failure.other:{"reason":"exit 3"}');
  });

  it("names an install from the catalog, else by its id", () => {
    const catalog = [{ id: "com.example.follow", name: "Follow me" }] as never;
    expect(installName("com.example.follow", catalog)).toBe("Follow me");
    expect(installName("com.example.other", catalog)).toBe("com.example.other");
  });

  it("accepts only http(s) archive URLs and an empty or 64-hex pin", () => {
    expect(isInstallUrl("https://x.example/a.adosplug")).toBe(true);
    expect(isInstallUrl("file:///etc/passwd")).toBe(false);
    expect(isSha256OrEmpty("")).toBe(true);
    expect(isSha256OrEmpty("ab")).toBe(false);
    expect(isSha256OrEmpty("a".repeat(64))).toBe(true);
  });

  it("the page offers Remove with keep-data and an install-from-URL field", () => {
    const route = read("routes/extensions-route.tsx");
    expect(route).toMatch(/removePlugin\(plugin\.plugin_id, keepData\)/);
    expect(route).toMatch(/<UrlInstall /);
    const row = read("components/plugins/installed-row.tsx");
    expect(row).toMatch(/disabled=\{incompatible\}/);
    expect(row).not.toMatch(/\{p\.status \?\?/);
  });
});
