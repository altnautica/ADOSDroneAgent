import { isKiosk } from "@/lib/kiosk";
import { isControllerSource, useInputStore } from "@/stores/input-store";

/** Whether focusing a text field should open the on-screen keyboard: on the
 *  kiosk panel, or when a pad or the panel's buttons drive the cockpit. */
export function useWantsKeyboard(): boolean {
  const controller = useInputStore((st) => isControllerSource(st.source));
  return isKiosk() || controller;
}
