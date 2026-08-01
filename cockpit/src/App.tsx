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
import { activeScreenId, useNavStore } from "@/stores/nav-store";

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
  // The active screen doubles as the boundary's reset signal: this boundary
  // wraps everything, so a caught fault takes the whole panel, and leaving it
  // permanent meant one bad screen ended the session. Navigating clears it.
  //
  // Read here rather than inside the boundary so the boundary stays a plain
  // component with no store dependency, and subscribed narrowly so this
  // re-renders on a screen change and nothing else.
  const activeTabId = useNavStore((s) => s.activeTabId);
  const detailStack = useNavStore((s) => s.detailStack);
  const screenId = activeScreenId({ activeTabId, detailStack });

  return (
    <ErrorBoundary resetKey={screenId}>
      <CockpitRoot />
    </ErrorBoundary>
  );
}
