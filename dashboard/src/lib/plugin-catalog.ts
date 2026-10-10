// View model for the "Available extensions" catalog grid. Pure so the error
// and update rules are unit-testable.

export interface CatalogEntry {
  id: string;
  name: string;
  version: string;
  description: string;
  author?: string;
  homepage?: string;
  license?: string;
  risk?: "low" | "medium" | "high" | "critical";
  category?: string;
  halves?: ("agent" | "gcs")[];
  download_url: string;
  archive_sha256?: string | null;
}

export interface CatalogResponse {
  schema_version: number;
  source: string;
  plugins: CatalogEntry[];
  /** Set when the bundled catalog failed to parse. */
  error?: string;
}

export type CatalogView =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "empty" }
  | { kind: "list"; entries: CatalogEntry[]; parseError: string | null };

/** What the grid shows for a catalog query. A fetch failure or a parse error
 *  is shown, never silently dropped. */
export function catalogView(
  data: CatalogResponse | undefined,
  error: unknown,
  isPending: boolean,
): CatalogView {
  if (error && !data) {
    const message = error instanceof Error && error.message ? error.message : "request failed";
    return { kind: "error", message: `Could not load the extension catalog: ${message}` };
  }
  if (!data) return isPending ? { kind: "loading" } : { kind: "empty" };
  const parseError = data.error ? `The bundled catalog could not be read: ${data.error}` : null;
  const entries = data.plugins ?? [];
  if (entries.length === 0) {
    return parseError ? { kind: "error", message: parseError } : { kind: "empty" };
  }
  return { kind: "list", entries, parseError };
}

export type InstallState = "absent" | "installed" | "update";

/** -1/0/1 for dotted numeric versions ("1.4.1" vs "1.10.0"); a pre-release or
 *  build suffix after `-`/`+` is ignored. */
export function compareVersions(a: string, b: string): number {
  const parts = (v: string) => v.split(/[-+]/)[0].split(".").map((n) => Number.parseInt(n, 10) || 0);
  const pa = parts(a);
  const pb = parts(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (d !== 0) return d < 0 ? -1 : 1;
  }
  return 0;
}

/** Whether a catalog entry is absent, installed at (or above) the catalog
 *  version, or installed at an older version that the catalog can update. */
export function installState(
  entry: Pick<CatalogEntry, "id" | "version">,
  installed: ReadonlyMap<string, string | undefined>,
): InstallState {
  if (!installed.has(entry.id)) return "absent";
  const have = installed.get(entry.id);
  return have && compareVersions(have, entry.version) < 0 ? "update" : "installed";
}
