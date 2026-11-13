// Where the Feed's main video shows: the grid renders this in the stage, the
// map view's card or the portrait frame, and it moves the Feed's one video
// node into itself (`useVideoHost`).

import { useLayoutEffect, useRef } from "react";

import type { VideoHost } from "@/hooks/use-video-host";

/** Fills its parent. */
export function VideoSlot({ attach }: { attach: VideoHost["attach"] }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => (ref.current ? attach(ref.current) : undefined), [attach]);
  return <div ref={ref} className="absolute inset-0" />;
}
