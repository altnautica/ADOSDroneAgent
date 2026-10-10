import { ApiError, credentialHeaders } from "@/shared/api-fetch";

/** One still frame and the moment the agent grabbed it. */
export interface Snapshot {
  blob: Blob;
  /** The agent's `X-Captured-At` stamp, or null when it sent none. */
  capturedAt: Date | null;
}

/**
 * A still frame grabbed on demand from the agent's live primary stream
 * (`GET /api/video/snapshot`), fetched with the dashboard's credential.
 *
 * Not an `<img src>`: an image element cannot send a header, and the agent
 * accepts a session in the URL only on the `/whep` and `/hls` media paths, so a
 * paired node reached off-box refuses a bare image request. A 503 means the
 * stream delivered no frame; the agent never substitutes an older one.
 */
export async function fetchSnapshot(signal?: AbortSignal): Promise<Snapshot> {
  const res = await fetch("/api/video/snapshot", {
    cache: "no-store",
    headers: credentialHeaders(),
    signal,
  });
  if (!res.ok) throw new ApiError(`snapshot ${res.status}`, res.status, null);
  const stamp = res.headers.get("X-Captured-At");
  const parsed = stamp ? new Date(stamp) : null;
  return {
    blob: await res.blob(),
    capturedAt: parsed && !Number.isNaN(parsed.getTime()) ? parsed : null,
  };
}

/** "Captured 14:02:31" in the viewer's local time. */
export function capturedLabel(at: Date): string {
  return `Captured ${at.toLocaleTimeString([], { hour12: false })}`;
}
