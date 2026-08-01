// The cockpit root. Mounts the shell and runs the three input paths (physical
// buttons, gamepad, and the touch focus that the shell renders) plus the screen
// wake lock and the UI-scale application. The button + gamepad hooks fold their
// events onto the one NavCommand set the navigator consumes, so all three
// sources drive the same menu.

import { CockpitShell } from "@/components/shell/cockpit-shell";
import { ErrorBoundary } from "@/components/error-boundary";
import { useButtons } from "@/hooks/use-buttons";
import { useGamepad } from "@/hooks/use-gamepad";
import { useUiScale } from "@/hooks/use-ui-scale";
import { useWakeLock } from "@/hooks/use-wake-lock";

/** The shell plus the hooks that feed it.
 *
 *  Split out from [`App`] so the error boundary sits ABOVE these hooks rather
 *  than beside them. A React error boundary only catches errors thrown BELOW
 *  it, and every hook here touches a browser API that can be absent or refuse
 *  on a kiosk panel (`navigator.getGamepads`, `navigator.wakeLock`, the
 *  WebSocket constructor, `document.documentElement`). With the boundary inside
 *  `App`, anything they threw escaped straight to the root and blanked the
 *  screen — on a panel with no console and no way to read the stack. */
function CockpitRoot() {
  useUiScale();
  const { connected: buttonsConnected } = useButtons();
  const { connected: gamepadConnected } = useGamepad();
  const { held: wakeHeld } = useWakeLock();

  return (
    <CockpitShell
      input={{ buttonsConnected, gamepadConnected, wakeHeld }}
    />
  );
}

export function App() {
  return (
    <ErrorBoundary>
      <CockpitRoot />
    </ErrorBoundary>
  );
}
