import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/** A raised charcoal surface panel — the default container for a framed screen
 *  body. Scrolls its own overflow (the document never scrolls; kiosk rule). */
export function Panel({
  className,
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "flex h-full w-full flex-col overflow-y-auto rounded-lg bg-surface/60 p-[0.6rem]",
        className,
      )}
    >
      {children}
    </div>
  );
}

/** A screen title header row. */
export function PanelHeader({
  title,
  right,
}: {
  title: string;
  right?: ReactNode;
}) {
  return (
    <div className="mb-[0.5rem] flex items-center justify-between gap-[0.5rem]">
      <h1 className="text-[1.15rem] font-semibold tracking-tight text-surface-foreground">
        {title}
      </h1>
      {right}
    </div>
  );
}
