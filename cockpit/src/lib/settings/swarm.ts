// The Swarm settings page model: the built-in formation generators the config
// model accepts (a name outside this set is refused), and the field bounds.

export const FORMATIONS = ["line", "column", "wedge", "grid", "circle"] as const;
export type Formation = (typeof FORMATIONS)[number];

export const FORMATION_LABEL = {
  line: { labelKey: "settingsMore.swarm.formationLine", descriptionKey: "settingsMore.swarm.formationLineDesc" },
  column: { labelKey: "settingsMore.swarm.formationColumn", descriptionKey: "settingsMore.swarm.formationColumnDesc" },
  wedge: { labelKey: "settingsMore.swarm.formationWedge", descriptionKey: "settingsMore.swarm.formationWedgeDesc" },
  grid: { labelKey: "settingsMore.swarm.formationGrid", descriptionKey: "settingsMore.swarm.formationGridDesc" },
  circle: { labelKey: "settingsMore.swarm.formationCircle", descriptionKey: "settingsMore.swarm.formationCircleDesc" },
} as const satisfies Record<Formation, { labelKey: string; descriptionKey: string }>;

export const SWARM_KEYS = {
  enabled: "swarm.enabled",
  role: "swarm.role",
  formation: "swarm.default_formation",
  spacing: "swarm.default_spacing",
} as const;

/** The role placeholder is the config value itself, not copy. */
export const ROLE_PLACEHOLDER = "auto";

export const SPACING_BOUNDS = { min: 0, max: 10_000 } as const;

export function toFormation(v: unknown): Formation | null {
  return (FORMATIONS as readonly unknown[]).includes(v) ? (v as Formation) : null;
}
