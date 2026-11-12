// Whether a resource has no data to show because its read failed (as opposed
// to still loading, or answering with an empty list).
export function readFailed(r: { data: unknown; error: string | null }): boolean {
  return r.data == null && r.error != null;
}
