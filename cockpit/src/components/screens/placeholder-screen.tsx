import type { LucideIcon } from "lucide-react";

import { Panel, PanelHeader } from "@/components/ui/panel";

/** A framed screen body used by the tab screens whose full content is built in
 *  a later stage. It renders the screen's title, a one-line summary of what the
 *  screen will hold, and the data surfaces it will read — so the shell is fully
 *  navigable now and each screen has a real registered entry to grow into. */
export function PlaceholderScreen({
  title,
  icon: Icon,
  summary,
  reads,
}: {
  title: string;
  icon?: LucideIcon;
  summary: string;
  reads: string[];
}) {
  return (
    <Panel>
      <PanelHeader title={title} />
      <div className="flex flex-1 flex-col items-center justify-center gap-[0.75rem] text-center">
        {Icon ? (
          <Icon className="h-[2.2rem] w-[2.2rem] text-amber" aria-hidden />
        ) : null}
        <p className="max-w-[26rem] text-[0.95rem] text-surface-foreground">
          {summary}
        </p>
        <div className="flex flex-wrap justify-center gap-[0.4rem]">
          {reads.map((r) => (
            <span
              key={r}
              className="rounded-md bg-muted px-[0.5rem] py-[0.25rem] font-mono text-[0.72rem] text-muted-foreground"
            >
              {r}
            </span>
          ))}
        </div>
      </div>
    </Panel>
  );
}
