// The Feed's main video, kept alive across layout changes. The video layer
// renders once, through a portal, into a DOM node the Feed owns for its whole
// life; the grid places a `VideoSlot` wherever the main video goes (the stage,
// the map view's card, the portrait frame), and the slot moves that node into
// itself. A pilot-map swap or a rotation re-renders the grid and its slots,
// but the <video> and the transport playing into it stay, so the picture
// carries on instead of dialling again.
//
// The node changes parent inside one React commit (the old slot's cleanup,
// then the new slot's layout effect), so the browser never sees the element
// out of the document long enough to pause it. While no slot holds it (a
// layout with nowhere to show the video), `attached` is false and the
// transport idles, as it did when the video unmounted.

import { useCallback, useState } from "react";

export interface VideoHost {
  /** The node the video layer portals into. */
  host: HTMLDivElement;
  /** A slot holds the node, so the video is on screen. */
  attached: boolean;
  /** Move the node into `parent`; returns the detach. */
  attach: (parent: HTMLElement) => () => void;
}

export function useVideoHost(): VideoHost {
  const [host] = useState(() => {
    const el = document.createElement("div");
    el.className = "absolute inset-0";
    return el;
  });
  const [slots, setSlots] = useState(0);
  const attach = useCallback(
    (parent: HTMLElement) => {
      parent.appendChild(host);
      // A browser that paused the element on the move resumes it.
      for (const v of host.querySelectorAll("video")) if (v.paused && v.srcObject) void v.play().catch(() => undefined);
      setSlots((n) => n + 1);
      return () => {
        if (host.parentNode === parent) parent.removeChild(host);
        setSlots((n) => n - 1);
      };
    },
    [host],
  );
  return { host, attached: slots > 0, attach };
}
