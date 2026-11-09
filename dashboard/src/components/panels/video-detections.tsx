// Detection boxes over the dashboard's live video, in the cockpit's corner
// bracket style. While the panel shows live video and the page is visible it
// holds a detection lease on the primary camera (the engine detects only on a
// camera someone watches) and polls the newest batch at 5 Hz; a stale or
// absent batch draws nothing.

import { useEffect, useRef, useState } from "react";

import { apiFetch } from "@/shared/api-fetch";
import { layoutVideoBoxes, parseLatestDetections, type DetectionFrame } from "@/lib/video-detections";

const POLL_MS = 200;
const LEASE_RENEW_MS = 2000;
const LEASE_HOLD_S = 6;
const CAMERA_ID = "main";
/** One lease owner per page load, so two dashboard tabs never drop each other's lease. */
const OWNER = `dashboard:${Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0")}`;
const BOX_COLOR = "var(--hud-ink)";
const LOCKED_COLOR = "var(--hud-track)";

const demand = (holdS: number) =>
  apiFetch("/api/vision/demand", { method: "POST", body: { camera_id: CAMERA_ID, hold_s: holdS, owner: OWNER } });

export function VideoDetections({
  active,
  videoRef,
}: {
  active: boolean;
  videoRef: React.RefObject<HTMLVideoElement | null>;
}) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const [frame, setFrame] = useState<{ f: DetectionFrame; videoW: number; videoH: number } | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });

  useEffect(() => {
    const box = boxRef.current;
    if (!active || !box) return;
    const ro = new ResizeObserver(() => {
      const r = box.getBoundingClientRect();
      setSize({ w: r.width, h: r.height });
    });
    ro.observe(box);

    let inFlight: AbortController | null = null;
    const poll = () => {
      if (inFlight || document.visibilityState === "hidden") return;
      const ac = new AbortController();
      inFlight = ac;
      apiFetch<unknown>(`/api/vision/detections/latest?camera_id=${CAMERA_ID}`, { signal: ac.signal })
        .then((body) => {
          const f = parseLatestDetections(body, CAMERA_ID);
          const v = videoRef.current;
          setFrame(f ? { f, videoW: v?.videoWidth ?? 0, videoH: v?.videoHeight ?? 0 } : null);
        })
        .catch(() => setFrame(null))
        .finally(() => {
          if (inFlight === ac) inFlight = null;
        });
    };
    const renew = () => void demand(LEASE_HOLD_S).catch(() => undefined);
    renew();
    poll();
    const pollTimer = setInterval(poll, POLL_MS);
    const leaseTimer = setInterval(renew, LEASE_RENEW_MS);
    return () => {
      clearInterval(pollTimer);
      clearInterval(leaseTimer);
      inFlight?.abort();
      ro.disconnect();
      setFrame(null);
      // Best effort: an unanswered release still lapses after the hold.
      void demand(0).catch(() => undefined);
    };
  }, [active, videoRef]);

  const boxes = frame ? layoutVideoBoxes(frame.f, size.w, size.h, frame.videoW, frame.videoH) : [];

  return (
    <div ref={boxRef} className="pointer-events-none absolute inset-0" aria-hidden>
      <svg className="absolute inset-0 h-full w-full overflow-visible">
        {boxes.map((b) => {
          const color = b.locked ? LOCKED_COLOR : BOX_COLOR;
          return (
            <g key={b.key}>
              <path d={b.path} stroke="var(--hud-target-outline-color)" strokeWidth={4.5} fill="none" />
              <path d={b.path} stroke={color} strokeWidth={2.5} fill="none" />
              <text
                x={b.x}
                y={b.y - 4}
                fill={color}
                stroke="var(--hud-target-outline-color)"
                strokeWidth={3}
                paintOrder="stroke"
                fontSize={11}
                fontFamily="var(--hud-target-tag-font)"
                fontWeight={600}
                style={{ textTransform: "uppercase" }}
              >
                {b.label}
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}
