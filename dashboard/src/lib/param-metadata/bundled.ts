/**
 * Bundled parameter metadata — the instant, offline floor.
 *
 * Snapshots ship as same-origin static assets under `public/param-metadata/`
 * and are fetched lazily per connected firmware. Because they are local, the
 * bitmask/enum editor works with no network at all (the primary fix). A
 * missing/unparsable file degrades to an empty Map — never throws.
 *
 * @module protocol/param-metadata/bundled
 * @license GPL-3.0-only
 */

import { ungzip } from "pako";
import type { FirmwareType, ParamMetadata, ParamSnapshot } from "./types";
import { deserializeMetaMap } from "./types";

const BASE_PATH = "/param-metadata";

/** Per-firmware bundled file basename. ArduPilot splits per vehicle. */
export function bundledKeyFor(ft: FirmwareType): string | null {
  switch (ft) {
    case "ardupilot-copter": return "ardupilot-copter";
    case "ardupilot-plane":  return "ardupilot-plane";
    case "ardupilot-rover":  return "ardupilot-rover";
    case "ardupilot-sub":    return "ardupilot-sub";
    case "px4":              return "px4";
    case "inav":             return "inav";
    case "betaflight":       return "betaflight";
    default:                 return null;
  }
}

// Parsed-snapshot cache so a firmware's file is fetched + parsed once per session.
const bundledCache = new Map<string, Map<string, ParamMetadata>>();
const EMPTY = new Map<string, ParamMetadata>();

/** Load the bundled snapshot for a firmware. Always resolves (empty on miss). */
export async function loadBundled(ft: FirmwareType): Promise<Map<string, ParamMetadata>> {
  const key = bundledKeyFor(ft);
  if (!key) return EMPTY;
  const hit = bundledCache.get(key);
  if (hit) return hit;
  try {
    const res = await fetch(`${BASE_PATH}/${key}.json.gz`);
    if (!res.ok) {
      bundledCache.set(key, EMPTY);
      return EMPTY;
    }
    const buf = new Uint8Array(await res.arrayBuffer());
    const snap = JSON.parse(ungzip(buf, { to: "string" })) as ParamSnapshot;
    const map = deserializeMetaMap(snap.params ?? []);
    bundledCache.set(key, map);
    return map;
  } catch {
    // Missing file / parse error → no bundled floor for this firmware yet.
    bundledCache.set(key, EMPTY);
    return EMPTY;
  }
}
