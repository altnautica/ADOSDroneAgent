// How a flight-screen control answers a press while it is unavailable: it
// stays focusable and pressable (`aria-disabled`, dimmed), and the press shows
// its reason in the cockpit toast instead of doing nothing, on touch, pad and
// kiosk alike. Available, the press runs the control.

import { toast } from "@/shared/api/toast";

export function pressOrExplain(reason: string | null | undefined, run: () => void): void {
  if (reason) toast.info(reason);
  else run();
}
