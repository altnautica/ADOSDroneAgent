// The Home alert centre's side drawer and the flight chip's blocking rows.
// Both bring the alert rows, the setup client and its spec text, and the drawer
// the Radix dialog stack, so the status tiles load this module on first open
// rather than in the main bundle.

import { Dialog, DialogContent } from "@/components/ui/dialog";
import { CARD_ALERT_SKIN, type AlertActions } from "@/shared/hud/alert-centre";
import { AlertCentre, AlertRow } from "@/shared/hud/alert-rows";
import type { PillAlert } from "@/shared/hud/status-pill";

export function AlertDrawer({
  open,
  onOpenChange,
  alerts,
  snoozed,
  onSnooze,
  actions,
  titleId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  alerts: readonly PillAlert[];
  snoozed: ReadonlySet<string>;
  onSnooze: (key: string, on: boolean) => void;
  actions: AlertActions;
  titleId: string;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        aria-labelledby={titleId}
        aria-describedby={undefined}
        // A side drawer: the full height at the right edge.
        className="left-auto right-0 top-0 h-full max-w-md translate-x-0 translate-y-0 overflow-y-auto rounded-none border-y-0 border-r-0 p-4 pt-12 text-sm data-[state=closed]:zoom-out-100 data-[state=open]:zoom-in-100"
        data-testid="status-alerts"
      >
        <AlertCentre alerts={alerts} snoozed={snoozed} onSnooze={onSnooze} actions={actions} skin={CARD_ALERT_SKIN} titleId={titleId} />
      </DialogContent>
    </Dialog>
  );
}

/** The blocking readiness items as alert rows (parameter table and fix). */
export function BlockingRows({ alerts, actions }: { alerts: readonly PillAlert[]; actions: AlertActions }) {
  return (
    <ul className="flex flex-col">
      {alerts.map((a) => (
        <AlertRow key={a.key} alert={a} actions={actions} skin={CARD_ALERT_SKIN} snoozed={false} />
      ))}
    </ul>
  );
}
