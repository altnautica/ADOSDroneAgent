// The Advanced settings page: the structured logger's level (applied on
// selection) and the HAL board override (applied behind a guard).

import { advancedSectionSchema } from "@/shared/api/apply-actions";
import type { AgentConfig } from "@/shared/api/types";

export const LOG_LEVELS = ["debug", "info", "warning", "error", "critical"] as const;
export type LogLevel = (typeof LOG_LEVELS)[number];

/** The configured level, lower-cased; anything unrecognised reads `info`. */
export function logLevelFrom(config: AgentConfig | null | undefined): LogLevel {
  const raw = config?.logging?.level?.toLowerCase();
  return (LOG_LEVELS as readonly string[]).includes(raw ?? "") ? (raw as LogLevel) : "info";
}

export function boardOverrideFrom(config: AgentConfig | null | undefined): string {
  return config?.agent?.board_override ?? "";
}

/** A pick of the current level sends nothing. */
export function logLevelChange(next: LogLevel, current: LogLevel): { log_level: LogLevel } | null {
  return next === current ? null : { log_level: next };
}

/** The override body, or the first validation issue as `path: message`. An
 *  empty slug is valid: it clears the override (auto-detect). */
export function overrideRequest(
  draft: string,
): { ok: true; body: { board_override: string } } | { ok: false; error: string } {
  const body = { board_override: draft };
  const result = advancedSectionSchema.safeParse(body);
  if (result.success) return { ok: true, body };
  const first = result.error.issues[0];
  return { ok: false, error: first ? `${first.path.join(".")}: ${first.message}` : "invalid" };
}

/** The guard is live only for a changed, valid slug while nothing is in flight. */
export function canApplyOverride(draft: string, initial: string, busy: boolean): boolean {
  return !busy && draft !== initial && overrideRequest(draft).ok;
}
