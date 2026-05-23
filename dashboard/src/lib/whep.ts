// Minimal WHEP client — POSTs an SDP offer, applies the SDP answer,
// returns a session handle the caller can close on unmount.
//
// WHEP is a one-shot HTTP transaction:
//   POST <whep_url>  Content-Type: application/sdp  Body: <offer SDP>
//   201 Created
//   Location: <resource URL>      (used for DELETE on close)
//   Body: <answer SDP>
//
// We attach a video transceiver in recvonly mode so the agent's
// mediamtx instance knows we are a viewer, not a publisher.

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

export async function startWhep(
  whepUrl: string,
  videoEl: HTMLVideoElement,
  signal?: AbortSignal,
): Promise<WhepResult> {
  const pc = new RTCPeerConnection({
    bundlePolicy: "max-bundle",
  });

  // Video-only recvonly. Our stream carries H.264 video only — no
  // audio track. Offering an audio transceiver makes mediamtx
  // negotiate an audio m= section in the SDP answer that will never
  // deliver bytes; that has caused Chrome to mark the whole
  // PeerConnection as degraded in past sessions.
  pc.addTransceiver("video", { direction: "recvonly" });

  const stream = new MediaStream();
  videoEl.srcObject = stream;

  pc.ontrack = (ev) => {
    for (const t of ev.streams[0]?.getTracks() ?? [ev.track]) {
      stream.addTrack(t);
    }
  };

  let resourceUrl: string | null = null;

  const close = async () => {
    if (resourceUrl) {
      // The handshake-time `signal` is typically already aborted by the
      // time close() runs (cleanup aborts it before calling close).
      // Use a fresh, short-lived signal so the DELETE actually reaches
      // mediamtx and frees the consumer slot.
      const deleteAc = new AbortController();
      const t = setTimeout(() => deleteAc.abort(), 2000);
      try {
        await fetch(resourceUrl, {
          method: "DELETE",
          signal: deleteAc.signal,
        });
      } catch {
        // ignore — connection is being torn down anyway
      } finally {
        clearTimeout(t);
      }
    }
    pc.getSenders().forEach((s) => s.track?.stop());
    pc.close();
    if (videoEl.srcObject === stream) {
      videoEl.srcObject = null;
    }
  };

  try {
    const offer = await pc.createOffer({
      offerToReceiveVideo: true,
      offerToReceiveAudio: true,
    });
    await pc.setLocalDescription(offer);

    // Wait briefly for ICE candidates so the answer can address them
    // (mediamtx's WHEP server expects the offer to carry trickled
    // candidates in-band rather than via ICE trickle over a separate
    // path).
    await waitForIceGathering(pc, 1500);

    const localDesc = pc.localDescription;
    if (!localDesc) {
      pc.close();
      return { ok: false, error: "Failed to build SDP offer." };
    }

    const res = await fetch(whepUrl, {
      method: "POST",
      headers: { "Content-Type": "application/sdp" },
      body: localDesc.sdp,
      signal,
    });

    if (!res.ok) {
      pc.close();
      return { ok: false, error: `WHEP ${res.status}: ${res.statusText}` };
    }

    const location = res.headers.get("Location");
    if (location) {
      try {
        resourceUrl = new URL(location, whepUrl).toString();
      } catch {
        resourceUrl = null;
      }
    }

    const answerSdp = await res.text();
    await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });

    return {
      ok: true,
      session: { pc, resourceUrl, close },
    };
  } catch (err) {
    pc.close();
    if (videoEl.srcObject === stream) {
      videoEl.srcObject = null;
    }
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
    };
  }
}

function waitForIceGathering(pc: RTCPeerConnection, timeoutMs: number) {
  return new Promise<void>((resolve) => {
    if (pc.iceGatheringState === "complete") {
      resolve();
      return;
    }
    const timer = setTimeout(() => {
      pc.removeEventListener("icegatheringstatechange", onChange);
      resolve();
    }, timeoutMs);
    function onChange() {
      if (pc.iceGatheringState === "complete") {
        clearTimeout(timer);
        pc.removeEventListener("icegatheringstatechange", onChange);
        resolve();
      }
    }
    pc.addEventListener("icegatheringstatechange", onChange);
  });
}
