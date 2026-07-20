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

export function App() {
  useUiScale();
  const { connected: buttonsConnected } = useButtons();
  const { connected: gamepadConnected } = useGamepad();
  const { held: wakeHeld } = useWakeLock();

  return (
    <ErrorBoundary>
      <CockpitShell
        input={{ buttonsConnected, gamepadConnected, wakeHeld }}
      />
    </ErrorBoundary>
  );
}
