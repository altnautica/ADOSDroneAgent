// Runs the navigation feed for the Feed's lifetime: the live nav-stream
// socket plus the occupancy poll (or, in demo mode, the scripted mock), torn
// down and cleared on unmount so nothing stale lingers.

import { useEffect } from "react";

import { mockLayerStream } from "@/lib/mock-layers";
import { mockNavStream } from "@/lib/mock-nav";
import { connectNavStream } from "@/lib/nav-stream";
import { isDemoMode } from "@/lib/utils";

export function useNavStream(enabled: boolean): void {
  useEffect(() => {
    if (!enabled) return;
    if (isDemoMode()) {
      mockNavStream.start();
      mockLayerStream.start();
      return () => {
        mockLayerStream.stop();
        mockNavStream.stop();
      };
    }
    return connectNavStream();
  }, [enabled]);
}
