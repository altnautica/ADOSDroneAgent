import { ApiError, credentialHeaders } from "./api";

/**
 * The agent's latest still frame (`GET /api/video/snapshot.jpg`), fetched with
 * the dashboard's credential.
 *
 * Not an `<img src>`: an image element cannot send a header, and the agent
 * accepts a session in the URL only on the `/whep` and `/hls` media paths, so a
 * paired node reached off-box refuses a bare image request.
 */
export async function fetchSnapshot(signal?: AbortSignal): Promise<Blob> {
  const res = await fetch("/api/video/snapshot.jpg", {
    cache: "no-store",
    headers: credentialHeaders(),
    signal,
  });
  if (!res.ok) throw new ApiError(`snapshot ${res.status}`, res.status, null);
  return res.blob();
}
