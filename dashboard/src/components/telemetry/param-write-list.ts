import type { ParamMetadata } from "@/shared/api/param-metadata/types";

export interface StagedChange {
  name: string;
  from: number;
  to: number;
  /** The new value lies outside the documented range. */
  outOfRange: boolean;
}

/** The first `limit` staged writes, name order, each marked when it leaves
 *  the parameter's documented range. */
export function stagedChanges(
  drafts: ReadonlyMap<string, { original: number; draft: number }>,
  meta: ReadonlyMap<string, ParamMetadata> | undefined,
  limit = 10,
): { shown: StagedChange[]; more: number } {
  const all = [...drafts.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, d]) => {
      const range = meta?.get(name)?.range;
      return {
        name,
        from: d.original,
        to: d.draft,
        outOfRange: range != null && (d.draft < range.min || d.draft > range.max),
      };
    });
  return { shown: all.slice(0, limit), more: Math.max(0, all.length - limit) };
}
