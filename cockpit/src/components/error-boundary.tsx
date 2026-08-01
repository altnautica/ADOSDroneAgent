// A top-level error boundary so a render fault shows a readable message on the
// panel instead of a white screen (a kiosk has no dev console to inspect).
//
// # Recoverable, because it wraps the whole app
//
// This boundary sits above every screen, so anything it catches replaces the
// entire cockpit — the menu, the status strip and the clock included. That was
// permanent: the caught error was stored and nothing ever cleared it, so one
// fault on one screen ended the session and the only affordance left was a
// reload. On a node that was refusing requests, even that failed, and the panel
// stayed on the error surface until someone power-cycled it.
//
// It now clears when the operator navigates. A fault on one screen costs that
// screen, not the aircraft's whole panel, and if the fault is genuinely
// permanent the surface simply comes straight back — which is the honest
// outcome rather than a hidden one.
//
// `resetKey` is the caller's "we are somewhere else now" signal. React has no
// built-in reset for a class boundary; comparing a key across updates is the
// documented way to do it.

import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
  /** Changing this clears a caught error. The shell passes the active screen. */
  resetKey?: string;
}

interface State {
  error: Error | null;
  /** The key in force when the error was caught, so a later change clears it. */
  caughtAt: string | undefined;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, caughtAt: undefined };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  static getDerivedStateFromProps(props: Props, state: State): Partial<State> | null {
    if (state.error === null) {
      // Track the key while healthy so the comparison below has a baseline.
      return state.caughtAt === props.resetKey
        ? null
        : { caughtAt: props.resetKey };
    }
    // Holding an error: clear it only once the caller says we have moved.
    if (state.caughtAt !== props.resetKey) {
      return { error: null, caughtAt: props.resetKey };
    }
    return null;
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // eslint-disable-next-line no-console
    console.error("cockpit render error", error, info.componentStack);
  }

  render(): ReactNode {
    if (this.state.error) {
      return (
        <div className="flex h-full w-full flex-col items-center justify-center gap-[0.75rem] bg-background p-[1.5rem] text-center">
          <p className="text-[1.1rem] font-semibold text-err">Cockpit error</p>
          <p className="max-w-[30rem] font-mono text-[0.8rem] text-muted-foreground">
            {this.state.error.message}
          </p>
          <p className="max-w-[30rem] text-[0.75rem] text-muted-foreground">
            Choosing another screen clears this.
          </p>
          <button
            type="button"
            onClick={() => window.location.reload()}
            className="touch-target rounded-md bg-amber px-[1rem] text-amber-foreground"
          >
            Reload
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
