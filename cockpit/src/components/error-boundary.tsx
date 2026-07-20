// A top-level error boundary so a render fault shows a readable message on the
// panel instead of a white screen (a kiosk has no dev console to inspect).

import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
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
