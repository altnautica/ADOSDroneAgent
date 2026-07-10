/**
 * Betaflight full-settings metadata — the version-keyed catalog loader.
 *
 * The catalog (`public/param-metadata/bf-settings-<ver>.json.gz`, produced by
 * `scripts/param-metadata/bf-settings.mjs`) carries type / range / enum options
 * for the ~810 CLI settings, keyed by their lowercase CLI names. It is merged
 * with the curated `betaflight.json.gz` groups (feature bitmask etc.) so the
 * settings viewer renders enum dropdowns / ranged numerics around the values
 * read live over the CLI. A missing/mismatched catalog degrades to an empty
 * map — the viewer then shows CLI values without rich metadata (never throws).
 *
 * @module protocol/param-metadata/bf-settings
 * @license GPL-3.0-only
 */

import { ungzip } from "pako";
import type { ParamMetadata, ParamSnapshot } from "./types";
import { deserializeMetaMap } from "./types";
import { loadBundled } from "./bundled";
import { mergeMetaMaps } from "./merge";

const BASE_PATH = "/param-metadata";

/**
 * Shipped BF settings-catalog version keys, newest first. Update this when
 * `scripts/param-metadata/bf-settings.mjs` is regenerated for a new firmware
 * version and its `bf-settings-<ver>.json.gz` is committed.
 */
export const SHIPPED_BF_CATALOGS = ["2026.6"] as const;

/** Extract a `MAJOR.MINOR` (or CalVer `YEAR.MONTH`) key from a firmware version string. */
export function bfCatalogVersionKey(versionString?: string): string | null {
  if (!versionString) return null;
  const m = versionString.match(/(\d+)\.(\d+)(?:\.\d+)?/);
  return m ? `${m[1]}.${m[2]}` : null;
}

/** Pick the best shipped catalog for a connected FC: exact match, else the newest. */
export function pickBfCatalog(versionString?: string): string {
  const key = bfCatalogVersionKey(versionString);
  if (key && (SHIPPED_BF_CATALOGS as readonly string[]).includes(key)) return key;
  return SHIPPED_BF_CATALOGS[0]; // newest shipped — best-effort metadata
}

const catalogCache = new Map<string, Map<string, ParamMetadata>>();
const EMPTY = new Map<string, ParamMetadata>();

async function fetchCatalog(key: string): Promise<Map<string, ParamMetadata>> {
  const hit = catalogCache.get(key);
  if (hit) return hit;
  try {
    const res = await fetch(`${BASE_PATH}/bf-settings-${key}.json.gz`);
    if (!res.ok) {
      catalogCache.set(key, EMPTY);
      return EMPTY;
    }
    const buf = new Uint8Array(await res.arrayBuffer());
    const snap = JSON.parse(ungzip(buf, { to: "string" })) as ParamSnapshot;
    const map = deserializeMetaMap(snap.params ?? []);
    catalogCache.set(key, map);
    return map;
  } catch {
    catalogCache.set(key, EMPTY);
    return EMPTY;
  }
}

/**
 * Load the Betaflight full-settings metadata: the version-keyed catalog merged
 * with the curated `betaflight.json.gz` groups. Always resolves.
 */
export async function loadBfSettingsMetadata(versionString?: string): Promise<Map<string, ParamMetadata>> {
  const [catalog, curated] = await Promise.all([
    fetchCatalog(pickBfCatalog(versionString)),
    loadBundled("betaflight"),
  ]);
  // Curated groups overlay the catalog (their bitmask labels are authoritative).
  return mergeMetaMaps(catalog, curated);
}
