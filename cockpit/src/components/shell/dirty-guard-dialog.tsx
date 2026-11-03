// The in-app dirty guard's dialog: shown when the operator tries to leave a
// screen that holds unsaved drafts (back, tab switch). Discard runs the parked
// navigation; Keep editing stays put.

import { Dialog } from "@/components/ui/dialog";
import { useDirtyStore } from "@/stores/dirty-store";

export function DirtyGuardDialog() {
  const pending = useDirtyStore((s) => s.pending);
  const discard = useDirtyStore((s) => s.discardAndLeave);
  const keep = useDirtyStore((s) => s.keepEditing);
  if (!pending) return null;

  return (
    <Dialog
      title="Discard unsaved changes?"
      onDismiss={keep}
      dismiss={
        <button type="button" onClick={keep} className="touch-target rounded-lg px-[0.9rem] text-[0.85rem] text-muted-foreground hover:text-foreground">
          Keep editing
        </button>
      }
      confirm={
        <button
          type="button"
          onClick={discard}
          className="touch-target rounded-lg border border-err bg-err/20 px-[0.9rem] text-[0.85rem] font-semibold text-err"
        >
          Discard
        </button>
      }
    >
      This screen has edits that were not applied. Leaving drops them.
    </Dialog>
  );
}
