// The Home place card (`@/lib/place-card`): the place-class picker with its
// explain line, the "Confined" badge, and the route-check report. A press
// sends the class over the page's pilot link; a refusal shows under it.

import { useState } from "react";

import { Button } from "@/components/ui/button";
import { choosePlace, type PlaceCardModel } from "@/lib/place-card";
import type { ActionResult } from "@/lib/pilot-action";
import { cn } from "@/lib/utils";

const REPORT_TONE = {
  ok: "border-border",
  warn: "border-warn/60 text-warn",
  critical: "border-destructive/60 bg-destructive/10 text-destructive",
} as const;

export interface PlaceCardProps {
  card: PlaceCardModel;
  send: (action: string) => Promise<ActionResult>;
}

export function PlaceCard({ card, send }: PlaceCardProps) {
  const [refusal, setRefusal] = useState<string | null>(null);
  const report = card.routeCheck;
  return (
    <div className="flex flex-col gap-1.5" data-testid="place-card">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground">{card.title}</span>
        <div className="flex gap-1" role="radiogroup" aria-label={card.title}>
          {card.options.map((o) => (
            <Button
              key={o.cls}
              type="button"
              size="sm"
              variant={o.selected ? "default" : "outline"}
              role="radio"
              aria-checked={o.selected}
              data-action={o.action}
              data-testid={`place-${o.cls}`}
              onClick={() => {
                setRefusal(null);
                void choosePlace(o, send).then(setRefusal);
              }}
            >
              {o.label}
            </Button>
          ))}
        </div>
        {card.badge ? (
          <span className="rounded-full border border-warn/60 px-2 py-0.5 font-mono font-semibold text-warn" data-testid="confined-chip">
            {card.badge}
          </span>
        ) : null}
      </div>
      <span className="text-muted-foreground" data-testid="place-explain">
        {card.explain}
      </span>
      {report ? (
        <div
          role={report.severity === "critical" ? "alert" : "status"}
          data-testid="route-check"
          data-state={report.state}
          className={cn("flex flex-col gap-0.5 rounded-md border px-3 py-1.5", REPORT_TONE[report.severity])}
        >
          <span className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground">{report.title}</span>
          <span className="font-semibold" data-testid="route-check-line">
            {report.line}
          </span>
          {report.pinchAt ? (
            <span className="font-mono" data-testid="route-check-pinch">
              {report.pinchAt}
            </span>
          ) : null}
        </div>
      ) : null}
      {refusal ? (
        <span role="status" data-testid="place-refusal" className="text-destructive">
          {refusal}
        </span>
      ) : null}
    </div>
  );
}
