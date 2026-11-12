// View model for an installed extension row: its display name, status words
// and tone, and the sentence for why the host stopped it.

import type { TFunction } from "i18next";

import type { BadgeVariant } from "@/components/ui/badge";
import type { CatalogEntry } from "@/lib/plugin-catalog";

const STATUS_KEYS = {
  installed: "extensions.status.installed",
  enabled: "extensions.status.enabled",
  running: "extensions.status.running",
  disabled: "extensions.status.disabled",
  failed: "extensions.status.failed",
  incompatible: "extensions.status.incompatible",
} as const;

export function installStatusLabel(status: string | undefined, t: TFunction): string {
  return status && status in STATUS_KEYS
    ? t(STATUS_KEYS[status as keyof typeof STATUS_KEYS])
    : t("extensions.status.unknown");
}

/** A failed start is an error, an incompatible one a warning. */
export function installStatusVariant(status: string | undefined): BadgeVariant {
  if (status === "running" || status === "enabled") return "success";
  if (status === "failed") return "error";
  if (status === "incompatible") return "warning";
  return "neutral";
}

/** One sentence for the host's `failure_reason`; null when it has not failed. */
export function failureText(reason: string | null | undefined, t: TFunction): string | null {
  if (!reason) return null;
  if (reason.startsWith("tampered")) return t("extensions.failure.tampered");
  if (reason.startsWith("signer_revoked")) return t("extensions.failure.signerRevoked");
  return t("extensions.failure.other", { reason });
}

/** The catalog name for an installed id; the id when the catalog has none. */
export function installName(pluginId: string, catalog: readonly CatalogEntry[] | undefined): string {
  return catalog?.find((e) => e.id === pluginId)?.name ?? pluginId;
}

/** A typed archive URL is usable when it is http(s); the agent applies its
 *  own allowlist on top. */
export function isInstallUrl(raw: string): boolean {
  try {
    const u = new URL(raw.trim());
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
}

/** An optional pin: empty, or 64 hex characters. */
export function isSha256OrEmpty(raw: string): boolean {
  const s = raw.trim();
  return s === "" || /^[0-9a-fA-F]{64}$/.test(s);
}
