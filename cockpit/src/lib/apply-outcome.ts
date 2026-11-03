// Reading the agent's batch apply reply (`POST /api/v1/setup/apply`), so every
// settings screen reports a partial failure and its rollback the same way.
//
// The agent applies sections in order and rolls back every applied section
// when a later one fails. HTTP 200 does not mean applied: `overall` does. A
// section can also succeed and still need a restart (`data.restart_required`).

import type { ApplyResponse } from "@/shared/api/apply-actions";

export interface ApplyOutcome {
  ok: boolean;
  /** Sections the agent refused, with its message. */
  failed: { section: string; message: string }[];
  /** Sections that applied and were then undone because a later one failed. */
  rolledBack: string[];
  /** Whether any applied section needs a service restart to take effect. */
  restartRequired: boolean;
  /** One line for a toast. */
  summary: string;
}

export function readApplyResponse(res: ApplyResponse | null | undefined): ApplyOutcome {
  const sections = res?.sections ?? {};
  const failed = Object.entries(sections)
    .filter(([, s]) => !s.ok)
    .map(([section, s]) => ({ section, message: s.message || "refused" }));
  const rolledBack = res?.rolled_back ?? [];
  // A missing reply or a missing `overall` is never read as success.
  const ok = res?.overall === true && failed.length === 0;
  const restartRequired =
    ok && Object.values(sections).some((s) => s.ok && s.data != null && s.data.restart_required === true);
  let summary: string;
  if (ok) {
    summary = restartRequired ? "Applied. A restart is needed for it to take effect." : "Applied.";
  } else if (failed.length) {
    const first = failed[0];
    summary = `Not applied: ${first.section}: ${first.message}`;
    if (rolledBack.length) summary += ` (rolled back ${rolledBack.join(", ")})`;
  } else {
    summary = "Not applied: the agent did not confirm the change.";
  }
  return { ok, failed, rolledBack, restartRequired, summary };
}
