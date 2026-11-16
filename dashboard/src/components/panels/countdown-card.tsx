// The Home countdown card (`@/lib/countdown-card`): the pilot's banner, a
// supervised lift-off's line during a launch, the
// low-battery return countdown with Cancel, and the mission stall with Skip
// waypoint, Return and Land here. Each button sends its pilot action over the
// page's pilot link; a refusal or a timeout shows under the buttons.

import { useState } from "react";

import { Button } from "@/components/ui/button";
import type { CountdownButton, CountdownCardModel } from "@/lib/countdown-card";
import type { ActionResult } from "@/lib/pilot-action";
import { refusalText } from "@/shared/hud/refusal";

export interface CountdownCardProps {
  card: CountdownCardModel;
  send: (action: string) => Promise<ActionResult>;
}

function Actions({ buttons, onPress }: { buttons: readonly CountdownButton[]; onPress: (b: CountdownButton) => void }) {
  return (
    <div className="flex flex-wrap gap-2">
      {buttons.map((b) => (
        <Button
          key={b.action}
          type="button"
          size="sm"
          variant="outline"
          className="border-destructive/60"
          data-action={b.action}
          data-testid={`countdown-${b.action}`}
          onClick={() => onPress(b)}
        >
          {b.label}
        </Button>
      ))}
    </div>
  );
}

export function CountdownCard({ card, send }: CountdownCardProps) {
  const [refusal, setRefusal] = useState<string | null>(null);
  const press = (b: CountdownButton) => {
    setRefusal(null);
    void send(b.action).then((r) => setRefusal(r.ok ? null : `${b.label}: ${refusalText(r.err)}`));
  };
  return (
    <div className="flex flex-col gap-2" data-testid="countdown-card">
      {card.stall ? (
        <div
          role="alert"
          data-testid="countdown-stall"
          data-kind={card.stall.kind}
          data-severity={card.stall.severity}
          className="flex flex-col gap-2 rounded-md border border-destructive/60 bg-destructive/10 px-3 py-2"
        >
          <span className="text-sm font-semibold text-destructive">{card.stall.title}</span>
          <Actions buttons={card.stall.actions} onPress={press} />
        </div>
      ) : null}
      {card.lowBattery ? (
        <div
          role="alert"
          data-testid="countdown-low-battery"
          data-severity="critical"
          className="flex flex-col gap-2 rounded-md border border-destructive/60 bg-destructive/10 px-3 py-2"
        >
          <span className="text-sm font-semibold text-destructive">{card.lowBattery.title}</span>
          <span className="text-xs text-muted-foreground">{card.lowBattery.warning}</span>
          <Actions buttons={[card.lowBattery.cancel]} onPress={press} />
        </div>
      ) : null}
      {card.banner ? (
        <div role="status" data-testid="countdown-banner" className="rounded-md border border-border px-3 py-1.5 text-sm font-medium">
          {card.banner}
        </div>
      ) : null}
      {card.liftoff ? (
        <div role="status" data-testid="countdown-liftoff" className="rounded-md border border-border px-3 py-1.5 text-sm">
          {card.liftoff}
        </div>
      ) : null}
      {refusal ? (
        <span role="status" data-testid="countdown-refusal" className="text-xs text-destructive">
          {refusal}
        </span>
      ) : null}
    </div>
  );
}
