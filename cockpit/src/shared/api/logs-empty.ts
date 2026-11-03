// What the Logs page says when it has no entries to show. Kept free of React so
// it is unit-testable.

/** The `warning` `GET /api/logs` answers while the logging store is off or not
 *  reachable (it is off by default). */
export const STORE_UNAVAILABLE = "logging store unavailable";

export type LogsEmptyState =
  | { kind: "loading" }
  | { kind: "storeOff" }
  | { kind: "storeError"; warning: string }
  | { kind: "noMatch" }
  | { kind: "noEntries" };

export function logsEmptyState(input: {
  loading: boolean;
  filtered: boolean;
  /** `GET /api/logs` `warning`: set when the store could not answer. */
  warning: string | undefined;
}): LogsEmptyState {
  if (input.loading) return { kind: "loading" };
  if (input.warning === STORE_UNAVAILABLE) return { kind: "storeOff" };
  if (input.warning) return { kind: "storeError", warning: input.warning };
  if (input.filtered) return { kind: "noMatch" };
  return { kind: "noEntries" };
}
