// List/detail layout. Landscape shows both: the list takes `listWeight` of the
// width (kept between 280 and 420 px), a divider, then the detail. Portrait
// shows one pane: the detail when `showDetailInPortrait`, otherwise the list.
// Mirrors the Android ConsoleTwoPane.

import type { ReactNode } from "react";

import { useWidthClass } from "@/hooks/use-width-class";

export function TwoPane({
  list,
  detail,
  showDetailInPortrait,
  listWeight = 0.38,
}: {
  list: ReactNode;
  detail: ReactNode;
  showDetailInPortrait: boolean;
  listWeight?: number;
}) {
  const { portrait } = useWidthClass();
  if (portrait) {
    return <div className="h-full min-h-0 overflow-y-auto">{showDetailInPortrait ? detail : list}</div>;
  }
  return (
    <div className="flex h-full min-h-0">
      <div
        className="h-full shrink-0 overflow-y-auto"
        style={{ width: `clamp(280px, ${listWeight * 100}%, 420px)` }}
      >
        {list}
      </div>
      <div className="w-px shrink-0 bg-border" aria-hidden />
      <div className="h-full min-w-0 flex-1 overflow-y-auto">{detail}</div>
    </div>
  );
}
