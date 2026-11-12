// The swarm block's operator fields (`swarm.*` in the agent config). Gains are
// integer percentages (40 means 0.40); radii are whole metres. The bounds are
// the operator ranges Mission Control offers, inside what the agent accepts.

export type SwarmMode = "hold" | "flocking" | "formation";
export const SWARM_MODES: readonly SwarmMode[] = ["hold", "flocking", "formation"];

export interface SwarmBlock {
  enabled?: boolean;
  role?: string;
  mode?: SwarmMode;
  default_formation?: "line" | "column" | "wedge" | "grid" | "circle";
  default_spacing?: number;
  flock?: {
    cohesion?: number;
    alignment?: number;
    separation_gain?: number;
    radius_m?: number;
    neighbors?: number;
  };
  separation?: { radius_m?: number; hard_m?: number };
}

export const GAIN_PCT = { min: 0, max: 200 } as const;
export const SEPARATION_M = { min: 1, max: 200 } as const;
export const FLOCK_RADIUS_M = { min: 1, max: 500 } as const;
export const FLOCK_NEIGHBORS = { min: 1, max: 24 } as const;

/** The repulsion radius must stay above the hard floor (true while either
 *  value is unread). */
export function separationOrderOk(radiusM: number | undefined, hardM: number | undefined): boolean {
  return radiusM == null || hardM == null || radiusM > hardM;
}
