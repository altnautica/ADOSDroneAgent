// The flight-state chip's detail for the skill that owns the aircraft
// (`Orbit 8 m`). The cockpit reads the skill's echoed parameters from its
// extension store; the dashboard reads the skill's state topic from the
// plugin's GCS manifest (`GET /api/plugins/{id}`), then the parameters the
// plugin echoes on that topic (`GET /api/plugins/{id}/state`). Both end in
// `skillDetail`. Golden cases: `fixtures/skill-detail-cases.json`.

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** The chip's mode detail from a running skill's echoed parameters:
 *  follow-me's `Orbit 8 m` / `Follow 6 m`; null when nothing is known. */
export function skillDetail(echo: Readonly<Record<string, unknown>> | null | undefined): string | null {
  if (!isObj(echo)) return null;
  const metres = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? ` ${Math.round(v * 10) / 10} m` : "");
  if (echo.follow_mode === "orbit") return `Orbit${metres(echo.orbit_radius_m)}`;
  if (echo.follow_mode === "follow") return `Follow${metres(echo.follow_distance_m)}`;
  return null;
}

/** The state topic a plugin's manifest declares for one skill, or null. */
export function skillStateTopic(detail: unknown, skillId: string): string | null {
  const manifest = isObj(detail) && isObj(detail.manifest) ? detail.manifest : null;
  const gcs = manifest && isObj(manifest.gcs) ? manifest.gcs : null;
  const contributes = gcs && isObj(gcs.contributes) ? gcs.contributes : null;
  const skills = contributes && Array.isArray(contributes.skills) ? contributes.skills : [];
  for (const raw of skills) {
    if (!isObj(raw) || raw.id !== skillId) continue;
    const topic = isObj(raw.state) ? raw.state.topic : null;
    return typeof topic === "string" && topic !== "" ? topic : null;
  }
  return null;
}

/** The chip detail from a plugin state body (`{ topic: { payload } }`). */
export function skillDetailFromState(body: unknown, topic: string | null): string | null {
  if (!topic || !isObj(body) || !isObj(body[topic])) return null;
  const echo = (body[topic] as Record<string, unknown>).payload;
  return isObj(echo) ? skillDetail(echo) : null;
}
