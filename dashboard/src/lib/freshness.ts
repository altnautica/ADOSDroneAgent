// Live values never outlive their source: react-query keeps `data` after a
// failed read, so a panel shows a value only while its query is not in error
// and its last success is at most two poll intervals old.

export interface QueryAge {
  isError: boolean;
  dataUpdatedAt: number;
}

export function isLive(q: QueryAge, intervalMs: number, nowMs: number): boolean {
  return !q.isError && q.dataUpdatedAt > 0 && nowMs - q.dataUpdatedAt <= 2 * intervalMs;
}

/** The query's data while live, else undefined (shown as a dash). */
export function liveData<T>(q: QueryAge & { data: T | undefined }, intervalMs: number, nowMs: number): T | undefined {
  return isLive(q, intervalMs, nowMs) ? q.data : undefined;
}
