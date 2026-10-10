// Minimal WHEP client: POST an SDP offer, apply the answer, keep the resource
// URL from `Location` so the session can be DELETEd on close.
//
//   POST <whep_url>  Content-Type: application/sdp  Body: <offer SDP>
//   201 Created      Location: <resource URL>        Body: <answer SDP>
//
// One recvonly video transceiver (the stream carries no audio; offering an
// audio transceiver negotiates an m= section that never delivers bytes).

import { credentialHeaders } from "./api-fetch";
import { withMediaAuth } from "./media-auth";

export interface WhepSession {
  pc: RTCPeerConnection;
  resourceUrl: string | null;
  close: () => Promise<void>;
}

export interface WhepResult {
  ok: boolean;
  session?: WhepSession;
  error?: string;
}

/** The WHEP endpoint for one camera leg: `?camera=<id>` selects a concurrent
 *  leg on the agent's proxy; no id means the primary leg. */
export function whepUrlFor(base: string, cameraId?: string | null): string {
  if (!cameraId) return base;
  const sep = base.includes("?") ? "&" : "?";
  return `${base}${sep}camera=${encodeURIComponent(cameraId)}`;
}

/**
 * Resolve a WHEP `Location` header to an absolute URL. The agent answers with
 * a path (`/whep/<session>`), so it is resolved against the page's own URL; a
 * relative WHEP endpoint is not a valid base on its own, and resolving against
 * it threw, which silently skipped the DELETE and leaked the media server's
 * consumer slot on every teardown.
 */
export function resolveWhepResource(location: string, pageHref: string): string | null {
  try {
    return new URL(location, pageHref).toString();
  } catch {
    return null;
  }
}

const ICE_GATHER_MS = 1500;
const DELETE_TIMEOUT_MS = 2000;

export async function startWhep(
  whepUrl: string,
  videoEl: HTMLVideoElement,
  opts: { signal?: AbortSignal } = {},
): Promise<WhepResult> {
  const pc = new RTCPeerConnection({ bundlePolicy: "max-bundle" });
  pc.addTransceiver("video", { direction: "recvonly" });

  const stream = new MediaStream();
  videoEl.srcObject = stream;
  pc.ontrack = (ev) => {
    for (const t of ev.streams[0]?.getTracks() ?? [ev.track]) stream.addTrack(t);
  };

  let resourceUrl: string | null = null;
  let closed = false;

  const close = async () => {
    if (closed) return;
    closed = true;
    if (resourceUrl) {
      // The handshake signal is usually aborted by now; a fresh short-lived one
      // lets the DELETE reach the media server and free the consumer slot.
      const ac = new AbortController();
      const t = setTimeout(() => ac.abort(), DELETE_TIMEOUT_MS);
      try {
        await fetch(resourceUrl, {
          method: "DELETE",
          headers: credentialHeaders(),
          signal: ac.signal,
        });
      } catch {
        // Tearing down regardless.
      } finally {
        clearTimeout(t);
      }
    }
    // A recvonly connection has no senders; stop the receiver tracks and the
    // copies handed to the element's stream so no track outlives the session.
    pc.getReceivers().forEach((r) => r.track?.stop());
    stream.getTracks().forEach((t) => {
      t.stop();
      stream.removeTrack(t);
    });
    pc.close();
    if (videoEl.srcObject === stream) videoEl.srcObject = null;
  };

  try {
    await pc.setLocalDescription(await pc.createOffer());
    // The media server expects candidates in-band in the offer.
    await waitForIceGathering(pc, ICE_GATHER_MS);
    const localDesc = pc.localDescription;
    if (!localDesc) throw new Error("Failed to build SDP offer.");

    // Header first; the URL copy survives proxies and redirects that drop it.
    const res = await fetch(withMediaAuth(whepUrl), {
      method: "POST",
      headers: { "Content-Type": "application/sdp", ...credentialHeaders() },
      body: localDesc.sdp,
      signal: opts.signal,
    });
    if (!res.ok) throw new Error(`WHEP ${res.status}: ${res.statusText}`);

    const location = res.headers.get("Location");
    if (location) {
      const pageHref =
        typeof window !== "undefined" ? window.location.href : new URL(whepUrl).toString();
      resourceUrl = resolveWhepResource(location, pageHref);
    }

    await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
    return { ok: true, session: { pc, resourceUrl, close } };
  } catch (err) {
    await close();
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
}

function waitForIceGathering(pc: RTCPeerConnection, timeoutMs: number): Promise<void> {
  return new Promise<void>((resolve) => {
    if (pc.iceGatheringState === "complete") {
      resolve();
      return;
    }
    const done = () => {
      clearTimeout(timer);
      pc.removeEventListener("icegatheringstatechange", onChange);
      resolve();
    };
    const onChange = () => {
      if (pc.iceGatheringState === "complete") done();
    };
    const timer = setTimeout(done, timeoutMs);
    pc.addEventListener("icegatheringstatechange", onChange);
  });
}
