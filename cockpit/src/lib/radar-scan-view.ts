// What the Feed's proximity radar draws from the live planar scan: the scan
// dots rotated onto the radar's north-up dial, the display range (pinned by
// the operator or chosen automatically from the nearest return), and the
// bottom readout as a range plus a CLEAR / CAUTION / DANGER word. A scan older
// than SCAN_FRESH_MS draws nothing, so a dead link never paints live dots.

import {
  autoRadarScale,
  nearestScanM,
  radarReadout,
  radarScanDots,
  RADAR_MAX_DOTS,
  RADAR_MAX_DOTS_LOW_POWER,
  type RadarBand,
  type RadarScale,
  type RadarScaleSetting,
} from "@/shared/hud/radar-scan";
import type { NavScan } from "@/shared/hud/nav-types";

/** The scan arrives at 10 Hz; one second without one is a dead source. */
export const SCAN_FRESH_MS = 1000;
/** `nav.state` (and its `min_obstacle_m`) arrives at about 1 Hz. */
export const NAV_FRESH_MS = 3000;

/**
 * The scan turned onto a north-up dial: body angles are clockwise from the
 * nose, so adding the heading makes them clockwise from north. With no
 * heading the scan stays nose-up, matching the radar's plain-dot nose.
 */
export function northUpScan(scan: NavScan, headingDeg: number | null): NavScan {
  if (headingDeg === null) return scan;
  return { ...scan, angleStartRad: scan.angleStartRad + (headingDeg * Math.PI) / 180 };
}

export interface RadarViewInput {
  scan: NavScan | null;
  scanAgeMs: number;
  headingDeg: number | null;
  setting: RadarScaleSetting;
  /** Nearest FC OBSTACLE_DISTANCE sector (m), null when every sector is clear. */
  fcNearestM: number | null;
  /** True when the FC reports proximity sectors at all. */
  fcPresent: boolean;
  /** The companion's `nav.state.min_obstacle_m`, null when absent. */
  minObstacleM: number | null;
  navAgeMs: number;
  radiusPx: number;
  cx: number;
  cy: number;
  lowPower?: boolean;
}

export interface RadarView {
  scaleM: RadarScale;
  /** `AUTO 6 m` or `6 m`. */
  scaleLabel: string;
  dots: Array<{ x: number; y: number }>;
  /** Null when no obstacle source is live (the radar shows the heading). */
  readout: { range: string; word: string; band: RadarBand } | null;
}

export function buildRadarView(input: RadarViewInput): RadarView {
  const scan = input.scan && input.scanAgeMs <= SCAN_FRESH_MS ? input.scan : null;
  const scanNearest = nearestScanM(scan);
  const scaleM = input.setting === "auto" ? autoRadarScale(scanNearest) : input.setting;
  const dots = scan
    ? radarScanDots(
        northUpScan(scan, input.headingDeg),
        scaleM,
        input.radiusPx,
        input.cx,
        input.cy,
        input.lowPower ? RADAR_MAX_DOTS_LOW_POWER : RADAR_MAX_DOTS,
      )
    : [];

  const navLive = input.navAgeMs <= NAV_FRESH_MS;
  const minObstacle = navLive ? input.minObstacleM : null;
  const anyLive = scan !== null || input.fcPresent || minObstacle !== null;
  const r = radarReadout(input.fcNearestM, minObstacle, scanNearest);
  const readout = r.band !== null && r.word !== null
    ? { range: r.range, word: r.word, band: r.band }
    : anyLive
      ? { range: r.range, word: "CLEAR", band: "clear" as const }
      : null;

  return {
    scaleM,
    scaleLabel: input.setting === "auto" ? `AUTO ${scaleM} m` : `${scaleM} m`,
    dots,
    readout,
  };
}
